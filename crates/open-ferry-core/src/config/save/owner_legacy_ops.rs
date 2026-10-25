//! Offline replay recipes shared only by the new owner audit tests.
use super::*;
use crate::config::v8_edit::{V8Edit, V8Method};
use serde_json::{Value, json};

#[derive(Clone)]
pub(crate) enum Operation {
    Save(Box<Config>, bool),
    Nested(Vec<String>, String),
    Write,
    Layout(bool),
    Edit(V8Edit),
}
impl Operation {
    pub(crate) fn candidate(
        &self,
        data: &[u8],
    ) -> Result<(tree::ReceiptedYaml, tree::CommentSource), String> {
        match self {
            Self::Save(cfg, migrate) => {
                candidate_preserving_comments(data, cfg, *migrate).map_err(|e| e.to_string())
            }
            Self::Nested(keys, value) => {
                let keys: Vec<_> = keys.iter().map(String::as_str).collect();
                candidate_nested_scalar(data, &keys, value).map_err(|e| e.to_string())
            }
            Self::Write => candidate_write_file(data).map_err(|e| e.to_string()),
            Self::Layout(migrate) => {
                v8::owner_audit_candidate(data, *migrate).map_err(|e| e.to_string())
            }
            Self::Edit(edit) => {
                crate::config::v8_edit::owner_audit_candidate(data, edit).map_err(|e| e.to_string())
            }
        }
    }
    pub(crate) fn run(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        let out = match self {
            Self::Save(cfg, migrate) => render_preserving_comments(data, cfg, *migrate),
            Self::Nested(keys, value) => {
                let keys: Vec<_> = keys.iter().map(String::as_str).collect();
                render_nested_scalar(data, &keys, value)
            }
            Self::Write => render_write_file(data),
            Self::Layout(migrate) => {
                normalize_config_layout(data, *migrate).map(|(bytes, _)| bytes)
            }
            Self::Edit(edit) => {
                return crate::config::v8_edit::preview_v8(data, edit).map_err(|e| e.to_string());
            }
        }
        .map_err(|e| e.to_string())?;
        if !matches!(self, Self::Layout(_)) {
            Config::load_bytes(&out).map_err(|e| e.to_string())?;
        }
        Ok(out)
    }
}
fn nth(value: &Value, pass: usize) -> Option<&str> {
    value.as_str().or_else(|| {
        value
            .as_array()
            .and_then(|v| v.get(pass).or_else(|| v.last()))
            .and_then(Value::as_str)
    })
}
pub(crate) fn operation(data: &[u8], row: &Value, pass: usize) -> Result<Operation, String> {
    let op = row["op"].as_str().unwrap().trim_end_matches('2');
    let p = &row["params"];
    Ok(match op {
        "save" | "migrate" => {
            let text = if row["empty_config"] == true {
                ""
            } else {
                row["config"]
                    .as_str()
                    .unwrap_or(std::str::from_utf8(data).unwrap())
            };
            let mut cfg = Config::parse(text).map_err(|e| e.to_string())?;
            if let Some(value) = nth(&p["save_auth"], pass) {
                cfg.auth_dir = value.into();
            }
            Operation::Save(
                Box::new(cfg),
                row["migrate"].as_bool().unwrap_or(op == "migrate"),
            )
        }
        "nested" => {
            let keys = row
                .get("keys")
                .filter(|v| v.is_array())
                .unwrap_or(&p["nested_keys"])
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            Operation::Nested(keys, nth(&p["nested_values"], pass).unwrap().to_owned())
        }
        "raw" | "v8raw" | "raw-v8" => Operation::Write,
        "normalize" => Operation::Layout(true),
        "put" | "patch" | "delete" => {
            let path = p["paths"]
                .get(op)
                .unwrap_or(&p["path"])
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            let body = p["bodies"]
                .get(op)
                .or_else(|| p.get("body"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .as_bytes()
                .to_vec();
            Operation::Edit(V8Edit {
                method: match op {
                    "put" => V8Method::Put,
                    "patch" => V8Method::Patch,
                    _ => V8Method::Delete,
                },
                path,
                body,
                yaml: false,
            })
        }
        _ => panic!("unsupported audit recipe"),
    })
}
pub(crate) fn old_scope<T>(row: &Value, call: impl FnOnce() -> T) -> T {
    #[cfg(feature = "parity")]
    {
        let mut disabled = Vec::new();
        if row["disable_empty"] == true {
            disabled.push(parity::ablation::Protection::EmptyMappingPendingComment);
        }
        if row["disable_flow_feet"] == true {
            disabled.push(parity::ablation::Protection::MultipleFlowItemFeet);
        }
        parity::ablation::with_disabled(&disabled, call)
    }
    #[cfg(not(feature = "parity"))]
    {
        assert!(row["disable_empty"] != true && row["disable_flow_feet"] != true);
        call()
    }
}
pub(crate) fn history(row: &Value) -> Value {
    old_scope(row, || {
        let mut data = row["source"].as_str().unwrap().as_bytes().to_vec();
        let mut files = Vec::new();
        for pass in 0..if row["op"].as_str().unwrap().ends_with('2') {
            2
        } else {
            1
        } {
            let out = operation(&data, row, pass).and_then(|op| op.run(&data));
            match out {
                Ok(bytes) => {
                    files.push(String::from_utf8(bytes.clone()).unwrap());
                    data = bytes;
                }
                Err(error) => return json!({"files":files,"refused":true,"error":error}),
            }
        }
        json!({"files":files})
    })
}
#[test]
#[ignore = "offline capture of frozen main legacy rows"]
fn owner_capture_main_legacy_cases() {
    let rows: Vec<Value> = serde_json::from_slice(
        &std::fs::read(std::env::var_os("FU_OWNERS_LEGACY_INPUT").unwrap()).unwrap(),
    )
    .unwrap();
    let results: Vec<_> = rows
        .iter()
        .map(|row| json!({"id":row["id"],"result":history(row)}))
        .collect();
    std::fs::write(
        std::env::var_os("FU_OWNERS_LEGACY_OUTPUT").unwrap(),
        serde_json::to_vec_pretty(&results).unwrap(),
    )
    .unwrap();
}
/// Both public callers, including their exact refusal, backup and hash behavior.
pub(crate) fn caller_history(row: &Value) -> Value {
    use crate::config::v8_edit::{edit_v8_if, preview_v8};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let dir = crate::config::testing::TempDir::new();
    let path = dir.join("config.yaml");
    std::fs::write(&path, row["source"].as_str().unwrap()).unwrap();
    let mut steps = Vec::new();
    for pass in 0..if row["op"].as_str().unwrap().ends_with('2') {
        2
    } else {
        1
    } {
        let before = std::fs::read(&path).unwrap();
        let Operation::Edit(edit) = operation(&before, row, pass).unwrap() else {
            panic!("caller recipe")
        };
        let preview = preview_v8(&before, &edit);
        let expected = sha256_hex(&before);
        let checked = Arc::new(AtomicBool::new(false));
        let check = checked.clone();
        let condition = move |hash: &str| {
            check.store(true, Ordering::Relaxed);
            hash == expected
        };
        let written = edit_v8_if(&path, &edit, Some(&condition));
        assert!(checked.load(Ordering::Relaxed));
        let actual = std::fs::read(&path).unwrap();
        let preview = match preview {
            Ok(bytes) => json!({"file":String::from_utf8(bytes).unwrap()}),
            Err(error) => json!({"error":error.to_string()}),
        };
        let write = match written {
            Ok((_, hash)) => {
                assert!(hash == sha256_hex(&actual));
                assert!(std::fs::read(backup_path(&path)).unwrap() == before);
                if let Some(bytes) = preview["file"].as_str() {
                    assert!(bytes.as_bytes() == actual);
                }
                json!({"file":String::from_utf8(actual).unwrap(),"sha256":hash,"backup":true})
            }
            Err(error) => {
                assert!(actual == before);
                json!({"error":error.to_string(),"unchanged":true})
            }
        };
        let refused = write.get("error").is_some();
        steps.push(json!({"preview":preview,"write":write,"precondition":true}));
        if refused {
            break;
        }
    }
    json!({"steps":steps})
}
#[test]
#[ignore = "offline frozen-main capture for both edit callers"]
fn owner_capture_main_callers() {
    let rows: Vec<Value> = serde_json::from_slice(
        &std::fs::read(std::env::var_os("FU_OWNERS_CALLER_INPUT").unwrap()).unwrap(),
    )
    .unwrap();
    let result: Vec<_> = rows
        .iter()
        .map(|row| {
            let run = || caller_history(row);
            let main = if std::env::var_os("FU_OWNERS_CALLER_OFF").is_some() {
                tree::without_owner_protection(run)
            } else {
                run()
            };
            json!({"recipe":row,"main":main})
        })
        .collect();
    std::fs::write(
        std::env::var_os("FU_OWNERS_CALLER_OUTPUT").unwrap(),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
}

/// Compact fixture rows refer only to an earlier, byte-identical source.
pub(super) fn resolve_sources(mut rows: Vec<Value>) -> Vec<Value> {
    let mut sources = std::collections::HashMap::new();
    for row in &mut rows {
        let recipe = &mut row["recipe"];
        if let Some(source) = recipe["source"].as_str() {
            sources.insert(recipe["id"].as_str().unwrap().to_owned(), source.to_owned());
        } else {
            let source = sources
                .get(recipe["source_ref"].as_str().unwrap())
                .expect("fixture source reference must name an earlier source");
            recipe["source"] = Value::String(source.clone());
        }
    }
    rows
}
