use super::*;
use crate::common::vertex_partial_args::{Assembler, ProbeStep};

#[test]
fn round8_unheld_union_rows_skip_final_clean_without_changing_bytes() {
    const R: usize = 32;
    const L: usize = 128;
    const M: usize = 256;
    let branches: Vec<_> = (0..R)
        .map(|branch| {
            let required: Vec<_> = (0..L).map(|at| format!("missing_{branch}_{at}")).collect();
            json!({"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}},"required":required})
        })
        .collect();
    let schema =
        json!({"type":"object","properties":{"rows":{"type":"array","items":{"anyOf":branches}}}});
    let request = json!({"tools":[{"name":"rows_tool","input_schema":schema}]});
    let sent = json!({"tools":[{"functionDeclarations":[{"name":"rows_tool","parametersJsonSchema":schema}]}]});
    let schemas = CallSchemas::resolve("rows_tool", &request, &sent);
    let clean = |value: &Value| schemas.clean(value);
    let probe = |state: usize, step: ProbeStep<'_>| schemas.probe(state, step);
    let mut assembler = Assembler::default();
    let mut output = String::new();
    let mut rows = Vec::new();
    for at in 0..M {
        let a = format!("a{at}");
        let b = format!("b{at}");
        rows.push(json!({"a":a,"b":b}));
        let frame = json!({"willContinue":true,"partialArgs":[
            {"jsonPath":format!("$.rows[{at}].a"),"stringValue":a},
            {"jsonPath":format!("$.rows[{at}].b"),"stringValue":b}
        ]});
        let (delta, done) = assembler
            .push_indexed(&frame, Some(&clean), Some(&probe))
            .unwrap();
        assert!(!done);
        output.push_str(&delta);
    }
    let (delta, done) = assembler
        .push_indexed(&json!({}), Some(&clean), Some(&probe))
        .unwrap();
    output.push_str(&delta);
    assert!(done);
    // This exact byte assertion also passes before the held-node gate is added.
    assert_eq!(output, json!({"rows":rows}).to_string());
    assert_eq!(assembler.full_arguments().unwrap(), output);
    eprintln!(
        "R8_REVIEW R={R} L={L} M={M} bytes={} complete=true equal=true final_clean_calls={}",
        output.len(),
        assembler.final_clean_calls()
    );
    assert_eq!(assembler.final_clean_calls(), 0);
}
