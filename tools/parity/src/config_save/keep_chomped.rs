//! A named known difference, independent of equivalences and certification.
use crate::cases::Case;
use serde_json::{Value, json};

pub const REASON: &str =
    "upstream grows keep-chomped scalar trailing newlines; Open Ferry preserves the source value";

pub fn evidence(case: &Case, go: &Value, rust: &Value) -> Option<Value> {
    if go == rust || go.as_object()?.len() != 1 || rust.as_object()?.len() != 1 {
        return None;
    }
    let (gs, rs, steps) = (
        go["files"].as_array()?,
        rust["files"].as_array()?,
        case.options["steps"].as_array()?,
    );
    if gs.len() != steps.len() || rs.len() != steps.len() {
        return None;
    }
    let mut source = case.options["file"].as_str()?;
    let mut witnesses = Vec::new();
    for (index, ((g, r), step)) in gs.iter().zip(rs).zip(steps).enumerate() {
        match step["op"].as_str()? {
            "write" => source = step["body"].as_str()?,
            "save" | "nested" => {}
            _ => return None,
        }
        let (g, r) = (g.as_str()?, r.as_str()?);
        if g != r {
            if !open_ferry_core::config::save::parity::extra_keep_chomped_newlines(source, g, r) {
                return None;
            }
            witnesses.push(json!({"step":index,"source":source,"go":g,"rust":r}));
        }
        source = r;
    }
    (!witnesses.is_empty()).then(|| {
        json!({
            "kind":"known-upstream-keep-chomped-newlines", "case":case.name,
            "options":case.options, "original_go":go, "protected_rust":rust,
            "witnesses":witnesses
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_seed_12_history_preserves_all_four_snapshots() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../cases/testdata/config-save-keep-chomped-history.json"
        ))
        .unwrap();
        let case = Case::new("random-12-2229", "", "").with_options(fixture["options"].clone());
        let rust = crate::config_save::steps(&case).unwrap();
        assert_eq!(rust, fixture["open_ferry_output"]);
        let go = &fixture["upstream_output"];
        let proof = evidence(&case, go, &rust).expect("strict known difference");
        assert_eq!(proof["witnesses"].as_array().unwrap().len(), 4);
        for index in 0..4 {
            let mut near = go.clone();
            near["files"][index] =
                format!("{}# unrelated\n", go["files"][index].as_str().unwrap()).into();
            assert!(evidence(&case, &near, &rust).is_none());
        }
    }

    #[test]
    fn known_difference_requires_only_extra_keep_chomped_newlines() {
        let source =
            "plugins:\n  configs:\n    example:\n      text: >+\n        fictional note\n\n\n";
        let case = Case::new("keep-chomped-history", "", "").with_options(json!({
            "file":"", "steps":[{"op":"write","body":source},{"op":"save"}]
        }));
        let go = json!({"files":[format!("{source}\n"),format!("{source}\n\n")]});
        let rust = json!({"files":[source,source]});
        assert!(evidence(&case, &go, &rust).is_some());
        for text in [
            format!("{source}# unrelated\n"),
            source.replace("fictional", "different"),
            source.replace(">+", ">"),
            source.replace("text:", "other:"),
            source.replace("\n\n\n", "\n\n"),
        ] {
            let mut near = go.clone();
            near["files"][1] = text.into();
            assert!(evidence(&case, &near, &rust).is_none());
        }
        let mut wrong_rust = rust.clone();
        wrong_rust["files"][0] = format!("{source}\n").into();
        assert!(evidence(&case, &go, &wrong_rust).is_none());
        let mut incomplete = go.clone();
        incomplete["files"].as_array_mut().unwrap().pop();
        assert!(evidence(&case, &incomplete, &rust).is_none());
        let mut refused = go.clone();
        refused["error"] = "refused".into();
        assert!(evidence(&case, &refused, &rust).is_none());
    }
}
