use serde_json::{Value, json};

use super::*;

fn parse(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap()
}

// TestUserTurnDropsRefusesAnEmptiedTurnEvenWhenOtherTurnsRemain
#[test]
fn user_turn_drops_refuses_an_emptied_turn_even_when_other_turns_remain() {
    let mut drops = UserTurnDrops::default();
    drops.end_turn(1); // an earlier normal turn
    drops.drop_part("container_upload");
    drops.end_turn(0); // the turn the attachment emptied
    drops.end_turn(3); // a later normal turn must not clear the refusal

    let err = drops.err().unwrap();
    assert_eq!(err.part_type, "container_upload");
    assert_eq!(err.status_code(), 400);
    assert!(err.is_request_scoped());
}

// TestUserTurnDropsKeepsATurnThatStillHasSendableParts
#[test]
fn user_turn_drops_keeps_a_turn_that_still_has_sendable_parts() {
    let mut drops = UserTurnDrops::default();
    drops.drop_part("file");
    drops.end_turn(1);
    assert_eq!(drops.err(), None);
}

// TestUserTurnDropsIgnoresAnEmptyTurnWithoutADroppedPart
#[test]
fn user_turn_drops_ignores_an_empty_turn_without_a_dropped_part() {
    let mut drops = UserTurnDrops::default();
    drops.end_turn(0);
    assert_eq!(drops.err(), None);
}

// TestUserTurnDropsReportsTheFirstEmptiedTurn
#[test]
fn user_turn_drops_reports_the_first_emptied_turn() {
    let mut drops = UserTurnDrops::default();
    drops.drop_part("document");
    drops.drop_part("container_upload");
    drops.end_turn(0);
    drops.drop_part("file");
    drops.end_turn(0);
    assert_eq!(
        drops.err().unwrap().to_string(),
        "unsupported content part: document"
    );
}

// TestUserTurnDropsDoesNotLeakAFlagIntoTheNextTurn
#[test]
fn user_turn_drops_does_not_leak_a_flag_into_the_next_turn() {
    let mut drops = UserTurnDrops::default();
    drops.drop_part("file");
    drops.end_turn(1);
    drops.end_turn(0);
    assert_eq!(drops.err(), None);
}

// Not upstream's: the text with no type.
#[test]
fn unsupported_part_error_without_a_type() {
    assert_eq!(
        UnsupportedPartError::default().to_string(),
        "unsupported content part"
    );
}

// TestIsHTTPURL
#[test]
fn is_http_url_cases() {
    let cases = [
        ("https://example.test/a.png", true),
        ("HTTP://example.test/a.png", true),
        ("  https://example.test/a  ", true),
        ("gs://bucket/a.pdf", false),
        ("files/abc123", false),
        ("ftp://example.test/a.png", false),
        ("data:image/png;base64,aGk=", false),
        ("https://", false),
        ("", false),
    ];
    for (value, want) in cases {
        assert_eq!(is_http_url(value), want, "is_http_url({value:?})");
    }
}

// Not upstream's: hosts and ports Go's url.Parse refuses, and ones it takes.
#[test]
fn is_http_url_follows_go_host_rules() {
    let cases = [
        ("https://user:pass@example.test/a", true),
        ("https://example.test:8443/a", true),
        ("https://[::1]:8443/a", true),
        ("https://example.test:port/a", false),
        ("https://exa mple.test/a", false),
        ("https://exa\u{1}mple.test/a", false),
        ("https://[::1/a", false),
        ("https:example.test/a", false),
        ("https://example.test?q=1", true),
    ];
    for (value, want) in cases {
        assert_eq!(is_http_url(value), want, "is_http_url({value:?})");
    }
}

// TestUserRunRefusesATurnEmptiedByADroppedPart
#[test]
fn user_run_refuses_a_turn_emptied_by_a_dropped_part() {
    let mut run = UserRun::default();
    run.add();
    run.end(); // an earlier normal turn
    run.drop_part("image");
    run.end(); // the turn the attachment emptied
    run.add();
    run.end(); // a later normal turn must not clear the refusal
    assert_eq!(
        run.err().unwrap().to_string(),
        "unsupported content part: image"
    );
}

// TestUserRunKeepsATurnWithOtherSendableContent
#[test]
fn user_run_keeps_a_turn_with_other_sendable_content() {
    let mut run = UserRun::default();
    run.drop_part("audio");
    run.add();
    run.end();
    assert_eq!(run.err(), None);
}

// TestInteractionsAttachmentType
#[test]
fn interactions_attachment_type_cases() {
    let cases = [
        (r#"{"type":"image","uri":"gs://b/a.png"}"#, "image"),
        (r#"{"type":" Audio "}"#, "audio"),
        (r#"{"type":"text","text":"hi"}"#, ""),
        (r#"{"text":"hi"}"#, ""),
        (r#"{"inlineData":{"mimeType":"image/png"}}"#, "inlineData"),
        (r#"{"inline_data":{"mime_type":"image/png"}}"#, "inlineData"),
        (r#"{"fileData":{"mimeType":"image/png"}}"#, "fileData"),
        (r#"{"file_data":{"mime_type":"image/png"}}"#, "fileData"),
        (r#"{}"#, ""),
        (r#""plain string""#, ""),
        (
            r#"{"type":"image_url","image_url":{"url":""}}"#,
            "image_url",
        ),
    ];
    for (raw, want) in cases {
        assert_eq!(interactions_attachment_type(&parse(raw)), want, "{raw}");
    }
}

// TestCountSendableGeminiPartsIgnoresBlankText
#[test]
fn count_sendable_gemini_parts_ignores_blank_text() {
    let parts = [
        json!({"text":""}),
        json!({"text":"  \n\t"}),
        json!({"text":"hi"}),
        json!({"inlineData":{"mimeType":"image/png","data":"aGk="}}),
        json!({"fileData":{"mimeType":"image/png","fileUri":"gs://b/a.png"}}),
        json!({"functionResponse":{"name":"f","response":{}}}),
    ];
    assert_eq!(count_sendable_gemini_parts(&parts), 4);
}

// TestIsInteractionsInstructionStep
#[test]
fn is_interactions_instruction_step_cases() {
    let cases = [
        (
            r#"{"type":"user_input","role":"developer","content":"note"}"#,
            false,
            true,
        ),
        (
            r#"{"type":"user_input","role":"System","content":"note"}"#,
            false,
            true,
        ),
        (r#"{"role":"system","content":"note"}"#, false, true),
        (r#"{"type":"system","text":"note"}"#, false, true),
        (r#"{"type":"developer","text":"note"}"#, false, true),
        (
            r#"{"type":"user_input","role":"user","content":"hi"}"#,
            false,
            false,
        ),
        (r#"{"type":"user_input","content":"hi"}"#, false, false),
        (r#"{"type":"user_input","content":"hi"}"#, true, true),
        (
            r#"{"type":"user_input","role":"user","content":"hi"}"#,
            true,
            false,
        ),
        (r#"{"type":"model_output","content":"hi"}"#, true, false),
        (r#"{"role":"assistant","content":"hi"}"#, true, false),
        (r#"{"role":"user","type":"system"}"#, false, false),
        (r#""plain string""#, true, true),
        (r#""plain string""#, false, false),
    ];
    for (raw, inherited, want) in cases {
        assert_eq!(
            is_interactions_instruction_step(&parse(raw), inherited),
            want,
            "{raw} inherited={inherited}"
        );
    }
}
