use super::*;

#[test]
fn loose_numbers_follow_gjson_without_emitting_nonfinite_json() {
    assert_eq!(
        assemble(&[json!({"partialArgs":[
            {"jsonPath":"$.boolean","numberValue":true},
            {"jsonPath":"$.positive","numberValue":"+5"},
            {"jsonPath":"$.fraction","numberValue":".5"},
            {"jsonPath":"$.hex","numberValue":"0x1p2"},
            {"jsonPath":"$.invalid","numberValue":"not a number"},
            {"jsonPath":"$.exact","numberValue":"1E+2"},
            {"jsonPath":"$.huge","numberValue":"-1e9999"}]
        })]),
        r#"{"boolean":1,"positive":5,"fraction":0.5,"hex":4,"invalid":0,"exact":1E+2,"huge":-1e9999}"#
    );
    for number in ["+Inf", "NaN", "+1e9999"] {
        let mut assembler = Assembler::default();
        assert!(
            assembler
                .push(
                    &json!({"partialArgs":[{"jsonPath":"$.number","numberValue":number}]}),
                    None
                )
                .is_err()
        );
        assert!(assembler.full_arguments().is_err());
    }
}

#[test]
fn numeric_call_names_and_ids_use_gjson_identity() {
    let call = StreamingCall::new("1".into(), "1".into(), "1".into(), 0);
    assert!(call.can_continue(&json!({"name":1,"id":1,"partialArgs":[]})));
    assert!(!call.can_continue(&json!({"name":1,"id":2,"partialArgs":[]})));
    assert!(!call.can_continue(&json!({"name":2,"id":1,"partialArgs":[]})));
}

#[test]
fn numeric_json_path_is_invalid_not_a_last_string_end_marker() {
    let mut assembler = Assembler::default();
    assembler.push(&json!({"willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}), None).unwrap();
    assert!(
        assembler
            .push(&json!({"partialArgs":[{"jsonPath":5}]}), None)
            .is_err()
    );
    assert!(assembler.full_arguments().is_err());
}

fn wide_call() -> StreamingCall {
    let mut call = StreamingCall::new("wide".into(), String::new(), "wide".into(), 0);
    let parts: Vec<_> = (0..2048)
        .map(|index| json!({"jsonPath":format!("$.k{index:04}"),"stringValue":"x","willContinue":true}))
        .collect();
    call.assembler
        .push(&json!({"willContinue":true,"partialArgs":parts}), None)
        .unwrap();
    call
}

#[test]
fn repeated_markers_and_compatibility_checks_complete() {
    let mut call = wide_call();
    let marker = json!({"name":"wide","willContinue":true,"partialArgs":[{"jsonPath":"$.k2047"}]});
    for _ in 0..10_000 {
        assert!(call.can_continue(&marker));
        call.assembler.push(&marker, None).unwrap();
    }
    assert!(call.assembler.push(&json!({}), None).unwrap().1);
}

#[test]
fn round3_root_paths_are_compatible_before_push_rejects_them() {
    for path in [None, Some(""), Some("$"), Some("  $  ")] {
        let mut call = StreamingCall::new("weather".into(), String::new(), "call".into(), 0);
        call.assembler.push(&json!({"willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]}), None).unwrap();
        let mut part = json!({"stringValue":"x"});
        if let Some(path) = path {
            part["jsonPath"] = json!(path);
        }
        let next = json!({"name":"weather","partialArgs":[part]});
        assert!(call.can_continue(&next), "{path:?}");
        assert!(call.assembler.push(&next, None).is_err(), "{path:?}");
    }
}
