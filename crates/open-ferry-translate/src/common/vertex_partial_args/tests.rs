// Ported from CLIProxyAPI vertex_partial_args_test.go (64316127, MIT).
// Additional bounds and poison-on-error cases retain Open Ferry protections.

use super::*;
use serde_json::json;

mod review_tests;

fn assemble(chunks: &[Value]) -> String {
    let mut assembler = Assembler::default();
    let mut joined = String::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let (delta, done) = assembler.push(chunk, None).unwrap();
        assert_eq!(done, index + 1 == chunks.len());
        joined.push_str(&delta);
    }
    assert_eq!(assembler.full_arguments().unwrap(), joined);
    serde_json::from_str::<Value>(&joined).unwrap();
    joined
}

#[test]
fn complex_typed_arguments_and_escaped_strings() {
    let out = assemble(&[
        json!({"willContinue":true,"partialArgs":[
            {"jsonPath":"$.query","stringValue":"line1\n\"quoted\"\\slash\t","willContinue":true},
            {"jsonPath":"$.count","numberValue":0},
            {"jsonPath":"$.enabled","boolValue":false},
            {"jsonPath":"$.optional","nullValue":null},
            {"jsonPath":"$.nested.items[0].label","stringValue":"first"},
            {"jsonPath":"$.nested.items[0].score","numberValue":1.5}]}),
        json!({"partialArgs":[
            {"jsonPath":"$.query","stringValue":"end"},
            {"jsonPath":"$.nested.items[1].label","stringValue":"second"},
            {"jsonPath":"$.nested.items[1].active","boolValue":true}]}),
    ]);
    assert_eq!(
        out,
        "{\"query\":\"line1\\n\\\"quoted\\\"\\\\slash\\tend\",\"count\":0,\"enabled\":false,\"optional\":null,\"nested\":{\"items\":[{\"label\":\"first\",\"score\":1.5},{\"label\":\"second\",\"active\":true}]}}"
    );
}

#[test]
fn upstream_complex_sequence_streams_immediately_and_finishes_on_empty_call() {
    let chunks = [
        r#"{"name":"complex_tool","partialArgs":[{"jsonPath":"$.query","stringValue":"line1\n\"quoted\"\\slash\t","willContinue":true}],"willContinue":true}"#,
        r#"{"partialArgs":[{"jsonPath":"$.query","stringValue":"end","willContinue":true}],"willContinue":true}"#,
        r#"{"partialArgs":[{"jsonPath":"$.query"},{"jsonPath":"$.count","numberValue":0},{"jsonPath":"$.enabled","boolValue":false},{"jsonPath":"$.optional","nullValue":"NULL_VALUE"}],"willContinue":true}"#,
        r#"{"partialArgs":[{"jsonPath":"$.nested.items[0].label","stringValue":"first","willContinue":true},{"jsonPath":"$.nested.items[0].score","numberValue":1.5}],"willContinue":true}"#,
        r#"{"partialArgs":[{"jsonPath":"$.nested.items[0].label"},{"jsonPath":"$.nested.items[1].label","stringValue":"second"},{"jsonPath":"$.nested.items[1].active","boolValue":true}],"willContinue":true}"#,
        "{}",
    ];
    let mut assembler = Assembler::default();
    let mut joined = String::new();
    for (at, chunk) in chunks.iter().enumerate() {
        let (delta, done) = assembler
            .push(&crate::json::exact::from_str(chunk).unwrap(), None)
            .unwrap();
        if at == 0 {
            assert!(delta.contains(r#"line1\n\"quoted\"\\slash\t"#));
        }
        assert_eq!(done, at + 1 == chunks.len());
        joined.push_str(&delta);
    }
    assert_eq!(
        joined,
        r#"{"query":"line1\n\"quoted\"\\slash\tend","count":0,"enabled":false,"optional":null,"nested":{"items":[{"label":"first","score":1.5},{"label":"second","active":true}]}}"#
    );
    assert_eq!(assembler.full_arguments().unwrap(), joined);
}

#[test]
fn interleaved_open_strings_and_ancestor_revisit() {
    assert_eq!(
        assemble(&[
            json!({"willContinue":true,"partialArgs":[
            {"jsonPath":"$.a","stringValue":"A","willContinue":true},
            {"jsonPath":"$.b","stringValue":"B","willContinue":true}]}),
            json!({"partialArgs":[
            {"jsonPath":"$.b","stringValue":"C"},
            {"jsonPath":"$.a","stringValue":"D"}]})
        ]),
        r#"{"a":"AD","b":"BC"}"#
    );
    assert_eq!(
        assemble(&[
            json!({"willContinue":true,"partialArgs":[{"jsonPath":"$.a.x","numberValue":1}]}),
            json!({"willContinue":true,"partialArgs":[{"jsonPath":"$.b","numberValue":2}]}),
            json!({"partialArgs":[{"jsonPath":"$.a.y","numberValue":3}]})
        ]),
        r#"{"a":{"x":1,"y":3},"b":2}"#
    );
}

#[test]
fn arrays_are_sorted_and_sparse_gaps_are_filled_only_at_completion() {
    assert_eq!(
        assemble(&[
            json!({"willContinue":true,"partialArgs":[{"jsonPath":"$[1]","numberValue":2}]}),
            json!({"partialArgs":[{"jsonPath":"$[0]","numberValue":1}]})
        ]),
        "[1,2]"
    );
    assert_eq!(
        assemble(&[json!({"partialArgs":[{"jsonPath":"$[2]","numberValue":3}]})]),
        "[null,null,3]"
    );
}

#[test]
fn quoted_keys_decode_unicode_surrogates_quotes_and_backslashes() {
    assert_eq!(
        assemble(&[json!({"partialArgs":[
        {"jsonPath":r#"$['\u0061']"#,"numberValue":1},
        {"jsonPath":r#"$["\uD83D\uDE00"]"#,"stringValue":"smile"},
        {"jsonPath":r#"$['k\'ey\\name']"#,"boolValue":true}]})]),
        r#"{"a":1,"😀":"smile","k'ey\\name":true}"#
    );
    for path in [
        r#"$['\x41']"#,
        r#"$['\😀']"#,
        r#"$['\uD800']"#,
        "$[9999999999999999999999999999999999]",
        "$[-1]",
        "$.a[",
        "$..x",
    ] {
        assert!(parse_path(path).is_err(), "{path}");
    }
}

#[test]
fn snake_case_markers_and_exact_numbers() {
    assert_eq!(
        assemble(&[
            json!({"will_continue":true,"partial_args":[{"json_path":"$.a","string_value":"x","will_continue":true}]}),
            json!({"partial_args":[{"json_path":"$.a"},{"json_path":"$.b","number_value":"1E+2"},{"json_path":"$.c","bool_value":false},{"json_path":"$.d","null_value":null}]})
        ]),
        r#"{"a":"x","b":1E+2,"c":false,"d":null}"#
    );
    assert_eq!(assemble(&[json!({"willContinue":false})]), "{}");
    let fallback = json!({"name":"weather","args":{"city":"Paris"},"partialArgs":[]});
    assert!(!is_streaming(&fallback));
    assert_eq!(assemble(&[fallback]), r#"{"city":"Paris"}"#);
    assert!(!is_streaming(&json!({"args":{},"willContinue":false})));
    assert!(is_streaming(&json!({"willContinue":false})));
}

#[test]
fn continuation_identity_and_completed_values() {
    let first = json!({"name":"weather","id":"call_1","willContinue":true,"partialArgs":[{"jsonPath":"$.city","stringValue":"New ","willContinue":true}]});
    let mut call = StreamingCall::new("weather".into(), "call_1".into(), "call_1".into(), 0);
    call.assembler.push(&first, None).unwrap();
    assert!(call.can_continue(
        &json!({"name":"weather","partialArgs":[{"jsonPath":"$.city","stringValue":"York"}]})
    ));
    assert!(!call.can_continue(&json!({"name":"weather","id":"call_2","partialArgs":[]})));
    assert!(!call.can_continue(&json!({"name":"other","partialArgs":[]})));
    assert!(!call.can_continue(&json!({"name":"weather","args":{}})));
    assert!(call.can_continue(&json!({})));
    call.assembler
        .push(
            &json!({"partialArgs":[{"jsonPath":"$.city","stringValue":"York"}]}),
            None,
        )
        .unwrap();
    assert!(!call.can_continue(&json!({"name":"weather","partialArgs":[]})));
}

#[test]
fn depth_index_and_size_errors_poison_the_call() {
    assert!(parse_path(&format!("${}", ".a".repeat(MAX_DEPTH))).is_ok());
    assert!(parse_path(&format!("${}", ".a".repeat(MAX_DEPTH + 1))).is_err());
    assert!(parse_path("$[1024]").is_ok());
    assert!(parse_path("$[1025]").is_err());
    let mut assembler = Assembler::default();
    assembler.push(&json!({"willContinue":true,"partialArgs":[{"jsonPath":"$.a","stringValue":"ok","willContinue":true}]}), None).unwrap();
    assert!(
        assembler
            .push(
                &json!({"partialArgs":[{"jsonPath":"$.a.x","numberValue":1}]}),
                None
            )
            .is_err()
    );
    assert!(assembler.full_arguments().is_err());
    assert!(assembler.push(&json!({}), None).is_err());
    let huge = "\n".repeat(MAX_BYTES / 2);
    assert!(
        Assembler::default()
            .push(
                &json!({"partialArgs":[{"jsonPath":"$.a","stringValue":huge}]}),
                None
            )
            .is_err()
    );
}

#[test]
fn aggregate_output_and_fallback_copy_limits_are_enforced() {
    let mut assembler = Assembler::default();
    for _ in 0..3 {
        let (_, done) = assembler.push(&json!({"willContinue":true,"partialArgs":[{"jsonPath":"$.a","stringValue":"x".repeat(MAX_BYTES / 4),"willContinue":true}]}), None).unwrap();
        assert!(!done);
    }
    assert!(assembler.push(&json!({"partialArgs":[{"jsonPath":"$.a","stringValue":"x".repeat(MAX_BYTES / 4)}]}), None).is_err());
    assert!(assembler.full_arguments().is_err());
    assert!(assembler.full.is_empty());
    let mut fallback = Assembler::default();
    assert!(
        fallback
            .push(&json!({"args":{"a":"x".repeat(MAX_BYTES + 1)}}), None)
            .is_err()
    );
    assert!(fallback.full_arguments().is_err());
    for call in [
        json!({"partialArgs":{}}),
        json!({"partialArgs":[false]}),
        json!({"partialArgs":[{"stringValue":"missing path"}]}),
    ] {
        assert!(Assembler::default().push(&call, None).is_err());
    }
}

#[test]
fn cleaner_probes_hold_placeholders_but_preserve_declared_values() {
    let clean = |value: &Value| {
        let mut value = value.clone();
        if let Some(object) = value.as_object_mut() {
            object.remove("_");
        }
        value
    };
    let mut assembler = Assembler::default();
    let (first, done) = assembler
        .push(
            &json!({"willContinue":true,"partialArgs":[
        {"jsonPath":"$._","boolValue":true},
        {"jsonPath":"$.reason","stringValue":"ke","willContinue":true}]}),
            Some(&clean),
        )
        .unwrap();
    assert!(!done);
    assert_eq!(first, r#"{"reason":"ke"#);
    let (last, done) = assembler
        .push(
            &json!({"partialArgs":[{"jsonPath":"$.reason","stringValue":"pt"}]}),
            Some(&clean),
        )
        .unwrap();
    assert!(done);
    assert_eq!(first + &last, r#"{"reason":"kept"}"#);
}

mod round5_tests;

#[test]
fn round7_five_thousand_keyed_children_complete_in_linear_lookups() {
    let start = std::time::Instant::now();
    let mut assembler = Assembler::default();
    let mut expected = serde_json::Map::new();
    let mut output = String::new();
    for at in 0..5_000 {
        let key = format!("k{at:04}");
        output.push_str(&assembler.push(&json!({"willContinue":true,"partialArgs":[{"jsonPath":format!("$.{key}"),"stringValue":"x"}]}),None).unwrap().0);
        assert!(
            assembler
                .find(&[Segment::Key(key.clone())])
                .unwrap()
                .is_some()
        );
        expected.insert(key, json!("x"));
    }
    let (delta, done) = assembler.push(&json!({}), None).unwrap();
    output.push_str(&delta);
    assert!(done);
    assert_eq!(output, Value::Object(expected).to_string());
    let lookups = assembler.lookup_steps.get();
    assert!(
        (10_000..=15_000).contains(&lookups),
        "key lookup steps={lookups}"
    );
    eprintln!(
        "R7_ASSEMBLY keys=5000 complete=true equal=true lookup_steps={lookups} elapsed_ms={}",
        start.elapsed().as_millis()
    );
}

#[test]
fn upstream_allows_more_than_sixty_five_thousand_argument_nodes() {
    let mut assembler = Assembler::default();
    let parts: Vec<_> = (0..65_537)
        .map(|i| json!({"jsonPath":format!("$.k{i}"),"nullValue":true}))
        .collect();
    assert!(
        assembler
            .push(&json!({"partialArgs":parts}), None)
            .unwrap()
            .1
    );
    assert_eq!(
        serde_json::from_str::<Value>(assembler.full_arguments().unwrap())
            .unwrap()
            .as_object()
            .unwrap()
            .len(),
        65_537
    );
}
