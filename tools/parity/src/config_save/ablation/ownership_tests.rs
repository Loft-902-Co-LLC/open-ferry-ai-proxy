use super::*;

#[test]
fn oracle_requests_bind_each_protected_source_and_complete_operation() {
    let case = Case::new("binding", "", "").with_options(json!({"file":"a: 1\n","steps":[{"op":"nested","keys":["a"],"value":"2"},{"op":"save","migrate":true},{"op":"write","body":"b: 3\n"}]}));
    let protected = json!({"files":["a: '2'\n","config-version: 8\n","b: 3\n"]});
    let mut requests = Vec::new();
    let bound = ownership::bind(&case, &protected, &mut |request| {
        let witness = request.options["_ownership_request"].clone();
        requests.push(witness.clone());
        Ok(GoResult::Output(
            serde_json::to_vec(
                &json!({"source":witness["source"],"operation":witness["operation"],"moves":[]}),
            )
            .unwrap(),
        ))
    })
    .unwrap();
    assert_eq!(requests.len(), 3);
    for (i, expected) in ["a: 1\n", "a: '2'\n", "b: 3\n"].into_iter().enumerate() {
        assert_eq!(requests[i]["source"], expected);
        assert_eq!(requests[i]["operation"], case.options["steps"][i]);
        assert_eq!(bound.options["_ownership_evidence"][i]["source"], expected);
    }
    assert!(case.options.get("_ownership_evidence").is_none());
}

#[test]
fn unavailable_identity_oracle_preserves_the_strict_failure() {
    let source = "disable-cooling: false # own\n";
    let protected = "routing:\n  cooldown:\n    disable-cooling: false # own\n";
    let case = Case::new("unavailable", "", "")
        .with_options(json!({"file":source,"steps":[{"op":"save","migrate":true}]}));
    let bound = ownership::bind(&case, &json!({"files":[protected]}), &mut |_| {
        Ok(GoResult::Panic("unavailable".into()))
    })
    .unwrap();
    let strict = core::comment_ownership_analysis(source, None, protected, &[]);
    let analyzed = ownership::analyze(&bound, 0, source, None, protected, &[], true);
    assert!(!analyzed.accepted());
    assert_eq!(analyzed.failure, strict.failure);
    assert!(analyzed.rules.is_empty());
}
