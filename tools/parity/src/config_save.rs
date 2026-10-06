//! Our side of the harness's `config-save/steps` entry: the case's file
//! written to a temporary directory, then each step run on it with
//! `open_ferry_core::config::save` (see `go/parity_config_save.go`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};

use open_ferry_core::config::Config;
use open_ferry_core::config::save::{save_preserving_comments, update_nested_scalar, write_file};
use serde_json::{Value, json};

use crate::cases::Case;

/// `config-save/steps`: `{"files": [...]}`, the file after each step, with
/// `"error"` added when a step failed.
pub fn steps(case: &Case) -> Result<Value, String> {
    let dir = TempDir::new()?;
    let path = dir.0.join("config.yaml");
    let file = case.options["file"].as_str().unwrap_or_default();
    fs::write(&path, file).map_err(|err| format!("write {}: {err}", path.display()))?;
    let mut files = Vec::new();
    for step in case.options["steps"].as_array().into_iter().flatten() {
        if let Some(message) = run_step(&path, step)? {
            return Ok(json!({ "error": message, "files": files }));
        }
        let data = fs::read(&path).map_err(|err| format!("read {}: {err}", path.display()))?;
        files.push(String::from_utf8_lossy(&data).into_owned());
    }
    Ok(json!({ "files": files }))
}

/// Runs one step on the file at `path`, and returns why it failed.
fn run_step(path: &Path, step: &Value) -> Result<Option<String>, String> {
    let text = |name: &str| step[name].as_str().unwrap_or_default();
    let result = match text("op") {
        "save" => {
            let source = match step["config"].as_str() {
                Some(config) => config.to_owned(),
                None => fs::read_to_string(path)
                    .map_err(|err| format!("read {}: {err}", path.display()))?,
            };
            let cfg = match Config::parse(&source) {
                Ok(cfg) => cfg,
                Err(err) => return Ok(Some(format!("config: {err}"))),
            };
            save_preserving_comments(path, &cfg, step["migrate"].as_bool().unwrap_or(false))
        }
        "nested" => {
            let keys: Vec<&str> = step["keys"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|key| key.as_str().unwrap_or_default())
                .collect();
            update_nested_scalar(path, &keys, text("value"))
        }
        "write" => write_file(path, text("body").as_bytes()),
        op => return Err(format!("unknown config-save op {op:?}")),
    };
    Ok(result.err().map(|err| err.to_string()))
}

/// A directory of its own under the system's temporary directory, removed
/// when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self, String> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let index = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!(
            "open-ferry-parity-config-save-{}-{index}",
            process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
        Ok(Self(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
