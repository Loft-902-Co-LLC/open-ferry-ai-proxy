//! The direct scalar-loss proof extended to yaml.v3's folded lookahead.
//! Whole successful files are required. Counterfactual requirements are unchanged.
use crate::cases::Case;
use serde_json::{Value, json};

fn segments(text: &str) -> Vec<(bool, String)> {
    let mut out: Vec<(bool, String)> = Vec::new();
    for line in text.split_inclusive('\n') {
        let archive = line.starts_with("# ") || line == "#\n" || line == "#";
        let body = if archive {
            line.strip_prefix("# ").unwrap_or(&line[1..])
        } else {
            line
        };
        if let Some((kind, previous)) = out.last_mut()
            && *kind == archive
        {
            previous.push_str(body);
        } else {
            out.push((archive, body.into()));
        }
    }
    out
}

pub fn evidence(case: &Case, go: &Value, rust: &Value) -> Option<Value> {
    if go == rust || go.as_object()?.len() != 1 || rust.as_object()?.len() != 1 {
        return None;
    }
    let (gs, rs, steps) = (
        go["files"].as_array()?,
        rust["files"].as_array()?,
        case.options["steps"].as_array()?,
    );
    // A save emits once, and a newly archived entry is emitted a second time.
    // Longer histories and raw writes need their own replay, so stay Different.
    if steps.len() != 1
        || gs.len() != steps.len()
        || gs.len() != rs.len()
        || !matches!(steps[0]["op"].as_str(), Some("save" | "nested"))
    {
        return None;
    }
    let source = case.options["file"].as_str()?;
    let mut configs = Vec::new();
    let mut paths = Vec::new();
    let mut proved = 0;
    let mut decoded = Vec::new();
    for ((g, r), step) in gs.iter().zip(rs).zip(steps) {
        match step["op"].as_str()? {
            "save" => {
                if let Some(config) = step.get("config") {
                    configs.push(config.as_str()?);
                }
            }
            "nested" => paths.push(
                step["keys"]
                    .as_array()?
                    .iter()
                    .map(|k| k.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()?,
            ),
            _ => return None,
        }
        let (g, r) = (g.as_str()?, r.as_str()?);
        if g == r {
            continue;
        }
        let (ga, ra) = (segments(g), segments(r));
        if ga.len() != ra.len() {
            return None;
        }
        for ((gkind, g), (rkind, r)) in ga.iter().zip(&ra) {
            if gkind != rkind {
                return None;
            }
            if g == r {
                continue;
            }
            let passes = if *gkind { 2 } else { 1 };
            if (*gkind && step["op"] != "save")
                || !open_ferry_core::config::save::parity::folded_scalar_reencode(
                    &[source],
                    g,
                    r,
                    &configs,
                    &paths,
                    passes,
                )
            {
                return None;
            }
            proved += 1;
            decoded.push(json!({"archived":gkind,"emitter_passes":passes,
                "go_folded_values":open_ferry_core::config::save::parity::folded_scalar_values(g)?,
                "rust_folded_values":open_ferry_core::config::save::parity::folded_scalar_values(r)?}));
        }
    }
    (proved > 0).then(||json!({"kind":"folded-scalar-reencode","case":case.name,"options":case.options,"original_go":go,"rust_original":rust,"proved_segments":proved,"decoded_scalars":decoded}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_whole_folded_cases_require_exact_replay_and_keep_near_misses_different() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../cases/testdata/config-save-folded-emission.json"
        ))
        .unwrap();
        for fixture in fixtures.as_array().unwrap() {
            let case = Case::new(fixture["case"].as_str().unwrap(), "", "")
                .with_options(fixture["options"].clone());
            let go = &fixture["upstream_output"];
            let rust = &fixture["open_ferry_output"];
            assert_eq!(crate::config_save::steps(&case).unwrap(), *rust);
            assert!(evidence(&case, go, rust).is_some(), "{}", case.name);
            let mut missing = go.clone();
            missing["files"].as_array_mut().unwrap().clear();
            let mut error = go.clone();
            error["error"] = "yaml: failed".into();
            let mut outside = go.clone();
            outside["files"][0] =
                format!("{}# unrelated\n", go["files"][0].as_str().unwrap()).into();
            let mut value = go.clone();
            value["files"][0] = go["files"][0]
                .as_str()
                .unwrap()
                .replace("keep", "changed")
                .into();
            let mut comment = go.clone();
            comment["files"][0] = format!("# moved\n{}", go["files"][0].as_str().unwrap()).into();
            for near in [missing, error, outside, value, comment] {
                assert!(evidence(&case, &near, rust).is_none());
                let result = crate::evaluate(
                    crate::translator::Translator::ConfigSave,
                    &case,
                    &crate::upstream::GoResult::Output(serde_json::to_vec(&near).unwrap()),
                );
                assert!(matches!(result.outcome, crate::Outcome::Different(_)));
            }
            let mut wrong_source = case.clone();
            wrong_source.options["file"] = case.options["file"]
                .as_str()
                .unwrap()
                .replace("keep", "other")
                .replace("folded", "different")
                .into();
            assert!(evidence(&wrong_source, go, rust).is_none());
            let mut longer = case.clone();
            longer.options["steps"]
                .as_array_mut()
                .unwrap()
                .push(json!({"op":"save"}));
            let mut go_longer = go.clone();
            go_longer["files"]
                .as_array_mut()
                .unwrap()
                .push(go["files"][0].clone());
            let mut rust_longer = rust.clone();
            rust_longer["files"]
                .as_array_mut()
                .unwrap()
                .push(rust["files"][0].clone());
            assert!(evidence(&longer, &go_longer, &rust_longer).is_none());
            let mut edit = case.clone();
            edit.options["steps"][0]["config"] =
                "unknown-styles: {folded: requested}\napi-keys: [requested]\n".into();
            assert!(evidence(&edit, go, rust).is_none());
        }
    }
}
