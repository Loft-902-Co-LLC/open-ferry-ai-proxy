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
fn thousands_of_root_keys_and_many_markers_exhaust_shared_work_cleanly() {
    let mut call = wide_call();
    let marker = json!({"willContinue":true,"partialArgs":[{"jsonPath":"$.k2047"}]});
    let mut rejected = false;
    for _ in 0..10_000 {
        if call.assembler.push(&marker, None).is_err() {
            rejected = true;
            break;
        }
    }
    assert!(
        rejected,
        "marker lookups must spend the aggregate allowance"
    );
    assert!(call.assembler.full_arguments().is_err());
    assert!(call.assembler.full.is_empty());
    assert!(call.assembler.push(&json!({}), None).is_err());
}

#[test]
fn compatibility_checks_spend_work_without_starting_a_fresh_call_on_exhaustion() {
    let mut call = wide_call();
    call.assembler.work = WorkBudget::default();
    call.assembler.work.charge(MAX_WORK - 1024).unwrap();
    let marker = json!({"name":"wide","willContinue":true,"partialArgs":[{"jsonPath":"$.k2047"}]});
    assert!(call.can_continue(&marker));
    assert!(call.assembler.work.check().is_err());
    assert!(call.assembler.full_arguments().is_err());
    assert!(call.assembler.push(&marker, None).is_err());
    assert!(call.assembler.full.is_empty());
}

#[test]
fn final_and_fallback_cleaners_cannot_hide_exhaustion() {
    for fallback in [false, true] {
        let mut assembler = Assembler::default();
        let clean = |value: &Value, work: &WorkBudget| {
            if value.get("actual").is_some() {
                let _ = work.charge(MAX_WORK + 1);
            }
            value.clone()
        };
        let chunk = if fallback {
            json!({"args":{"actual":1}})
        } else {
            json!({"partialArgs":[{"jsonPath":"$.actual","numberValue":1}]})
        };
        assert!(assembler.push(&chunk, Some(&clean)).is_err());
        assert!(assembler.full_arguments().is_err());
        assert!(assembler.full.is_empty());
    }
}
