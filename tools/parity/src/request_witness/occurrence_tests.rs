use super::*;
use crate::compare::{Deviation, compare_numbers_with_witness};

#[test]
fn request_witness_parts_do_not_borrow_an_equal_occurrence() {
    let rows: Vec<Value> = serde_json::from_str(include_str!(
        "../cases/testdata/request-occurrence-witness.json"
    ))
    .unwrap();
    let translator = Translator::RegistryRequest;
    let path = "/messages/0/content";
    let mut results = Vec::new();
    for row in rows {
        let name = row["case"].as_str().unwrap();
        let case = Case::new(name, "m", row["input"]["request"].as_str().unwrap())
            .with_options(row["input"]["options"].clone());
        let native = translator.run_rust(&case).unwrap();
        for defect in [false, true] {
            let compact_client = |out: &mut Value| {
                let text = out
                    .pointer(path)
                    .and_then(Value::as_str)
                    .unwrap()
                    .replace(r#"{ "a": 1 }"#, r#"{"a":1}"#)
                    .replace(r#"{ "b": 2 }"#, r#"{"b":2}"#);
                *out.pointer_mut(path).unwrap() = text.into();
            };
            let mut baseline = native.clone();
            if defect {
                compact_client(&mut baseline);
            }
            let mut go = baseline.clone();
            *go.pointer_mut(path).unwrap() = row["go"].pointer(path).unwrap().clone();
            // Plant the same output defect in the baseline and its marker
            // replays, without changing any translator implementation.
            let witness = |p: &str, s: &str, t: &str, at: usize| {
                bound_span_replaying(&case, &baseline, p, s, t, at, |changed| {
                    let mut out = translator.run_rust(changed)?;
                    if defect {
                        compact_client(&mut out);
                    }
                    Ok(out)
                })
            };
            let compared = compare_numbers_with_witness(
                &go,
                &baseline,
                translator.embedded_json(&case),
                translator.numbers(),
                Some(&case.request),
                Some(&witness),
            );
            let should_differ = defect && name != "structured-only";
            results.push(json!({"name":name,"defect":defect,"expected_different":should_differ,"different":!compared.differences.is_empty(),"embedded_json":compared.deviations.contains(&Deviation::EmbeddedJson)}));
        }
    }
    if let Some(path) = std::env::var_os("PARITY_REQUEST_OCCURRENCE_EVIDENCE") {
        std::fs::write(path, serde_json::to_vec_pretty(&results).unwrap()).unwrap();
    }
    assert!(
        results
            .iter()
            .all(|r| r["expected_different"] == r["different"]),
        "{results:?}"
    );
    assert!(
        results
            .iter()
            .filter(|r| r["defect"] == false && r["name"].as_str().unwrap().starts_with("both-"))
            .all(|r| r["embedded_json"] == true)
    );
}

#[test]
fn copied_source_positions_can_each_be_witnessed_without_borrowing_another() {
    let compact = r#"{"a":1}"#;
    let probe = r#"{"marker":"unique"}"#;
    let text = format!("é{compact} and {compact}");
    let both = text.replace(compact, probe);
    let changed = changed_occurrences(&text, &both, compact, probe).unwrap();
    assert_eq!(
        changed.into_iter().collect::<Vec<_>>(),
        vec![2, 2 + compact.len() + 5]
    );
    let one = text.replacen(compact, probe, 1);
    assert_eq!(
        changed_occurrences(&text, &one, compact, probe)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert!(changed_occurrences(&text, &format!("{one}!"), compact, probe).is_none());
}
