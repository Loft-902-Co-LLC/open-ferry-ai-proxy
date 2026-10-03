// Ported from CLIProxyAPI internal/translator/common/openai_tools_test.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;

fn parse_all(messages: &[&str]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| serde_json::from_str(message).unwrap())
        .collect()
}

fn align(messages: &[&str]) -> Vec<Value> {
    align_openai_tool_call_messages(parse_all(messages), [])
}

fn roles(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .map(|message| str_of(message.get("role")).into_owned())
        .collect()
}

fn text(message: &Value, key: &str) -> String {
    str_of(message.get(key)).into_owned()
}

/// Asserts that `messages` come back exactly as they went in.
fn assert_untouched(messages: &[&str], history: &str) {
    let got = align(messages);
    assert_eq!(got.len(), messages.len(), "{history} history: {got:?}");
    for (i, (got, want)) in got.iter().zip(messages).enumerate() {
        assert_eq!(
            got.to_string(),
            *want,
            "{history} history should be untouched, but differed at {i}"
        );
    }
}

#[test]
fn empty_or_single_message_unchanged() {
    // Go passes nil and expects nil back; the port's empty Vec stays empty.
    assert!(align(&[]).is_empty(), "expected no messages");
    let single = [r#"{"role":"user","content":"hi"}"#];
    assert_eq!(
        align(&single),
        parse_all(&single),
        "expected single message unchanged"
    );
}

#[test]
fn already_adjacent_messages_remain_unchanged() {
    let messages = [
        r#"{"role":"user","content":"start"}"#,
        r#"{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"f1"}}]}"#,
        r#"{"role":"tool","tool_call_id":"call_1","content":"res1"}"#,
        r#"{"role":"assistant","content":"done"}"#,
    ];
    let got = align(&messages);
    assert_eq!(got.len(), messages.len(), "messages count");
    for (i, (got, want)) in got.iter().zip(messages).enumerate() {
        assert_eq!(got.to_string(), want, "mismatch at {i}");
    }
}

#[test]
fn reorders_separated_tool_results_to_follow_assistant() {
    let got = align(&[
        r#"{"role":"user","content":"start"}"#,
        r#"{"role":"assistant","tool_calls":[{"id":"call_a","type":"function"},{"id":"call_b","type":"function"}]}"#,
        r#"{"role":"assistant","content":"thinking part","reasoning_content":"step 1"}"#,
        r#"{"role":"user","content":"system reminder"}"#,
        r#"{"role":"tool","tool_call_id":"call_b","content":"res_b"}"#,
        r#"{"role":"tool","tool_call_id":"call_a","content":"res_a"}"#,
        r#"{"role":"user","content":"final prompt"}"#,
    ]);
    assert_eq!(got.len(), 7, "messages count");

    // The results follow the assistant in their own order; the rest keep
    // theirs.
    assert_eq!(
        roles(&got),
        [
            "user",
            "assistant",
            "tool",
            "tool",
            "assistant",
            "user",
            "user"
        ],
        "unexpected roles"
    );
    assert_eq!(text(&got[2], "tool_call_id"), "call_b", "got[2]");
    assert_eq!(text(&got[3], "tool_call_id"), "call_a", "got[3]");
    assert_eq!(text(&got[4], "reasoning_content"), "step 1", "got[4]");
    assert_eq!(text(&got[5], "content"), "system reminder", "got[5]");
}

#[test]
fn leaves_incomplete_tool_calls_untouched() {
    // The assistant calls call_1 and call_2, but only call_1 has a result.
    assert_untouched(
        &[
            r#"{"role":"assistant","tool_calls":[{"id":"call_1"},{"id":"call_2"}]}"#,
            r#"{"role":"user","content":"middle"}"#,
            r#"{"role":"tool","tool_call_id":"call_1","content":"res1"}"#,
        ],
        "incomplete",
    );
}

#[test]
fn leaves_orphan_tool_messages_untouched() {
    assert_untouched(
        &[
            r#"{"role":"user","content":"start"}"#,
            r#"{"role":"tool","tool_call_id":"call_orphan","content":"orphan"}"#,
        ],
        "orphan",
    );
}

#[test]
fn leaves_ambiguous_duplicate_call_ids_untouched() {
    assert_untouched(
        &[
            r#"{"role":"assistant","tool_calls":[{"id":"dup_call"}]}"#,
            r#"{"role":"assistant","tool_calls":[{"id":"dup_call"}]}"#,
            r#"{"role":"user","content":"middle"}"#,
            r#"{"role":"tool","tool_call_id":"dup_call","content":"res"}"#,
        ],
        "ambiguous",
    );
}

#[test]
fn leaves_assistant_with_mixed_empty_id_untouched() {
    // The assistant calls one empty ID and one valid call_a.
    assert_untouched(
        &[
            r#"{"role":"assistant","tool_calls":[{"id":""},{"id":"call_a"}]}"#,
            r#"{"role":"user","content":"reminder"}"#,
            r#"{"role":"tool","tool_call_id":"call_a","content":"res_a"}"#,
        ],
        "mixed empty ID",
    );
}

#[test]
fn preserves_numeric_precision_and_raw_byte_content() {
    let large_int_json = r#"{"custom_id":9223372036854775807,"precise_ratio":0.12345678901234567,"role":"assistant","tool_calls":[{"function":{"name":"p"},"id":"call_p","type":"function"}]}"#;
    let tool_json = r#"{"content":"ok","role":"tool","timestamp_ns":1789703256000000001,"tool_call_id":"call_p"}"#;
    let user_json = r#"{"content":"intervening","role":"user"}"#;

    let got = align(&[large_int_json, user_json, tool_json]);
    assert_eq!(got.len(), 3, "messages count");
    assert_eq!(
        got[0].to_string(),
        large_int_json,
        "assistant message changed"
    );
    assert_eq!(got[1].to_string(), tool_json, "tool message changed");
    assert_eq!(got[2].to_string(), user_json, "user message changed");
}

#[test]
fn leaves_causal_violation_untouched_when_result_precedes_call() {
    assert_untouched(
        &[
            r#"{"role":"tool","tool_call_id":"call_future","content":"res"}"#,
            r#"{"role":"assistant","tool_calls":[{"id":"call_future"}]}"#,
        ],
        "causal violation",
    );
}

#[test]
fn reorders_multiple_interleaved_assistant_turns_independently() {
    let got = align(&[
        r#"{"role":"assistant","tool_calls":[{"id":"call_1"}]}"#,
        r#"{"role":"user","content":"reminder 1"}"#,
        r#"{"role":"tool","tool_call_id":"call_1","content":"res 1"}"#,
        r#"{"role":"assistant","tool_calls":[{"id":"call_2"}]}"#,
        r#"{"role":"user","content":"reminder 2"}"#,
        r#"{"role":"tool","tool_call_id":"call_2","content":"res 2"}"#,
    ]);
    assert_eq!(got.len(), 6, "messages count");
    assert_eq!(
        roles(&got),
        ["assistant", "tool", "user", "assistant", "tool", "user"],
        "unexpected roles"
    );
    assert_eq!(text(&got[1], "tool_call_id"), "call_1", "got[1]");
    assert_eq!(text(&got[4], "tool_call_id"), "call_2", "got[4]");
}
