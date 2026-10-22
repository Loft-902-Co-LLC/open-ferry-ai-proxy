//! Exact source witness for the canonical version field's lost line comment.
use crate::cases::Case;
use crate::compare::Deviation;
use serde_json::{Value, json};

pub fn repair(case: &Case, go: &mut Value, rust: &Value) -> Option<Deviation> {
    if go == rust {
        return None;
    }
    let steps = case.options["steps"].as_array()?;
    let (gfiles, rfiles) = (go["files"].as_array()?, rust["files"].as_array()?);
    if gfiles.len() != rfiles.len() || gfiles.len() > steps.len() {
        return None;
    }
    let mut source = case.options["file"].as_str()?;
    let mut repaired = go.clone();
    let mut changed = false;
    for (i, ((g, r), step)) in gfiles.iter().zip(rfiles).zip(steps).enumerate() {
        match step["op"].as_str()? {
            "write" => source = step["body"].as_str()?,
            "save" => {}
            "nested" => {
                if step["keys"].as_array()?.first()?.as_str()? == "config-version" {
                    return None;
                }
            }
            _ => return None,
        }
        if let Some(value) = open_ferry_core::config::save::parity::config_version_comment(
            source,
            g.as_str()?,
            r.as_str()?,
        ) {
            repaired["files"][i] = value.into();
            changed = true;
        }
    }
    if !changed {
        return None;
    }
    *go = repaired;
    Some(Deviation::UpstreamVersionCommentLoss)
}
pub fn evidence(case: &Case, go: &Value, rust: &Value) -> Option<Value> {
    let mut comparable = go.clone();
    super::drop_unloadable(&mut comparable);
    super::drop_repeated_plugin_comments(case, &mut comparable);
    let before = comparable.clone();
    repair(case, &mut comparable, rust)?;
    Some(
        json!({"kind":"config-version-comment-loss","case":case.name,"options":case.options,"original_go":go,"rust_original":rust,"go_before_version_comments":before,"go_with_source_version_comments":comparable}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deferred_version_comment_histories_remain_different_without_a_field_witness() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../cases/testdata/config-save-version-history.json"
        ))
        .unwrap();
        for f in fixtures.as_array().unwrap() {
            let case =
                Case::new(f["case"].as_str().unwrap(), "", "").with_options(f["options"].clone());
            let go = crate::upstream::GoResult::Output(
                f["upstream_output_text"]
                    .as_str()
                    .unwrap()
                    .as_bytes()
                    .to_vec(),
            );
            let result = crate::evaluate(crate::translator::Translator::ConfigSave, &case, &go);
            assert_eq!(
                result.rust_output.as_ref().unwrap(),
                &f["open_ferry_output"]
            );
            assert!(matches!(result.outcome, crate::Outcome::Different(_)));
            let upstream: Value =
                serde_json::from_str(f["upstream_output_text"].as_str().unwrap()).unwrap();
            let rust = &f["open_ferry_output"];
            let mut changed = 0;
            for (g, r) in upstream["files"]
                .as_array()
                .unwrap()
                .iter()
                .zip(rust["files"].as_array().unwrap())
            {
                let (g, r) = (g.as_str().unwrap(), r.as_str().unwrap());
                open_ferry_core::config::Config::load_bytes(r.as_bytes()).unwrap();
                assert_eq!(
                    open_ferry_core::config::AnyValue::parse_yaml(g).unwrap(),
                    open_ferry_core::config::AnyValue::parse_yaml(r).unwrap()
                );
                if g == r {
                    continue;
                }
                let glines: Vec<_> = g.split_inclusive('\n').collect();
                let rlines: Vec<_> = r.split_inclusive('\n').collect();
                assert_eq!(glines.len(), rlines.len());
                let differences: Vec<_> = glines
                    .iter()
                    .zip(rlines)
                    .filter(|(g, r)| **g != *r)
                    .collect();
                assert_eq!(differences.len(), 1);
                let (gline, rline) = differences[0];
                assert_eq!(*gline, "config-version: 8\n");
                let comment = rline
                    .strip_prefix("config-version: 8 ")
                    .unwrap()
                    .trim_end_matches('\n');
                assert!(comment.starts_with('#'));
                assert_eq!(r.matches(comment).count(), 1);
                assert_eq!(g.matches(comment).count(), 0);
                changed += 1;
            }
            assert!(changed > 0);
        }
    }
    #[test]
    fn actual_go_raw_write_loses_its_source_version_comment() {
        let f: Value = serde_json::from_str(include_str!(
            "../cases/testdata/config-save-version-comment.json"
        ))
        .unwrap();
        let case = Case::new("version-go", "", "").with_options(f["options"].clone());
        let go = crate::upstream::GoResult::Output(f["upstream_output"].to_string().into_bytes());
        let e = crate::evaluate(crate::translator::Translator::ConfigSave, &case, &go);
        assert!(
            matches!(&e.outcome,crate::Outcome::Equivalent(d) if d.contains(&Deviation::UpstreamVersionCommentLoss))
        );
        let proof = evidence(
            &case,
            &f["upstream_output"],
            e.rust_output.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(proof["original_go"], f["upstream_output"]);
        let mut wrong = f["upstream_output"].clone();
        wrong["files"][0] = wrong["files"][0]
            .as_str()
            .unwrap()
            .replace("debug: false", "debug: true")
            .into();
        assert!(matches!(
            crate::evaluate(
                crate::translator::Translator::ConfigSave,
                &case,
                &crate::upstream::GoResult::Output(wrong.to_string().into_bytes())
            )
            .outcome,
            crate::Outcome::Different(_)
        ));
    }
    #[test]
    fn version_comment_loss_requires_exact_source_and_field_bytes() {
        let source = "config-version: 8 # owned\nx: same\n";
        let case = Case::new("version", "", "")
            .with_options(json!({"file":source,"steps":[{"op":"save"}]}));
        let go = json!({"files":["config-version: 8\nx: same\n"]});
        let rust = json!({"files":[source]});
        let mut patched = go.clone();
        assert_eq!(
            repair(&case, &mut patched, &rust),
            Some(Deviation::UpstreamVersionCommentLoss)
        );
        assert_eq!(patched, rust);
        for text in [
            source.replace("# owned", "# invented"),
            source.replace("8 #", "9 #"),
            source.replace("config-version:", "nested-version:"),
            source.replace("8 # owned", "8  # owned"),
        ] {
            let mut patched = go.clone();
            assert!(repair(&case, &mut patched, &json!({"files":[text]})).is_none());
        }
        let mut missing = case.clone();
        missing.options["file"] = "config-version: 8\nx: same\n".into();
        assert!(repair(&missing, &mut go.clone(), &rust).is_none());
        let mut targeted = case.clone();
        targeted.options["steps"] = json!([{"op":"nested","keys":["config-version"],"value":"8"}]);
        assert!(repair(&targeted, &mut go.clone(), &rust).is_none());
        let wrong = json!({"files":[source.replace("x: same","x: changed")]});
        let mut patched = go.clone();
        repair(&case, &mut patched, &wrong).unwrap();
        assert_ne!(patched, wrong);
        assert!(
            repair(
                &case,
                &mut json!({"files":["config-version: 8 # different\nx: same\n"]}),
                &rust
            )
            .is_none()
        );
    }

    #[test]
    fn duplicated_retained_version_comment_stays_different() {
        let source = "config-version: 8 # owned\nx: same\n";
        let case = Case::new("duplicate-version", "", "")
            .with_options(json!({"file":source,"steps":[{"op":"save"}]}));
        let mut go = json!({"files":["config-version: 8\nx: same # owned\n"]});
        let rust = json!({"files":["config-version: 8 # owned\nx: same # owned\n"]});
        assert!(repair(&case, &mut go, &rust).is_none());
        assert!(
            !crate::compare::compare(&go, &rust, &[])
                .differences
                .is_empty()
        );
    }
}
