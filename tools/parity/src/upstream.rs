//! Builds and runs the Go harness (`go/main.go`) inside a CLIProxyAPI checkout.

use std::env;
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use crate::cases::Case;

pub enum GoResult {
    Output(Vec<u8>),
    Panic(String),
}

pub struct Upstream {
    pub dir: PathBuf,
    /// `git describe` output for the checkout, such as `v8.0.11`.
    pub version: String,
    pub commit: String,
    harness: PathBuf,
}

impl Upstream {
    /// Compiles the harness into `work_dir`. The harness imports internal
    /// packages, so it must be compiled as part of the CLIProxyAPI module. An
    /// overlay adds it as `cmd/open-ferry-parity` without touching the checkout.
    pub fn build(dir: &Path, go: &Path, work_dir: &Path) -> Result<Self, Box<dyn Error>> {
        let dir = std::path::absolute(dir)?;
        if !dir.join("go.mod").is_file() {
            return Err(format!(
                "{} has no go.mod; pass a CLIProxyAPI checkout",
                dir.display()
            )
            .into());
        }
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("go")
            .join("main.go");
        let target = dir.join("cmd").join("open-ferry-parity").join("main.go");
        let mut replace = Map::new();
        replace.insert(
            target.to_string_lossy().into_owned(),
            source.to_string_lossy().into_owned().into(),
        );
        let overlay = work_dir.join("overlay.json");
        fs::write(&overlay, json!({ "Replace": replace }).to_string())?;

        let harness = work_dir.join(format!("upstream-harness{}", env::consts::EXE_SUFFIX));
        let status = Command::new(go)
            .current_dir(&dir)
            .arg("build")
            .arg("-overlay")
            .arg(&overlay)
            .arg("-o")
            .arg(&harness)
            .arg("./cmd/open-ferry-parity")
            .status()
            .map_err(|err| {
                format!(
                    "could not run {}: {err} (install Go or pass --go)",
                    go.display()
                )
            })?;
        if !status.success() {
            return Err(format!("go build failed ({status})").into());
        }

        Ok(Self {
            version: git(&dir, &["describe", "--tags", "--always", "--dirty"]),
            commit: git(&dir, &["rev-parse", "HEAD"]),
            dir,
            harness,
        })
    }

    /// Runs `translator` on every case. Results are in case order.
    pub fn run(
        &self,
        translator: &str,
        cases: &[Case],
        work_dir: &Path,
    ) -> Result<Vec<GoResult>, Box<dyn Error>> {
        let input_path = work_dir.join("input.jsonl");
        let mut input = BufWriter::new(File::create(&input_path)?);
        for case in cases {
            let line =
                json!({ "translator": translator, "model": case.model, "request": case.request });
            serde_json::to_writer(&mut input, &line)?;
            input.write_all(b"\n")?;
        }
        input
            .into_inner()
            .map_err(|err| err.into_error())?
            .sync_all()?;

        let output = Command::new(&self.harness)
            .stdin(File::open(&input_path)?)
            .stderr(Stdio::inherit())
            .output()?;
        if !output.status.success() {
            return Err(format!("upstream harness failed ({})", output.status).into());
        }
        let results = output
            .stdout
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .map(parse_result)
            .collect::<Result<Vec<_>, _>>()?;
        if results.len() != cases.len() {
            return Err(format!(
                "harness returned {} results for {} cases",
                results.len(),
                cases.len()
            )
            .into());
        }
        Ok(results)
    }
}

fn parse_result(line: &[u8]) -> Result<GoResult, Box<dyn Error>> {
    let line: Value = serde_json::from_slice(line)?;
    if let Some(message) = line.get("panic").and_then(Value::as_str) {
        return Ok(GoResult::Panic(message.to_owned()));
    }
    let output = match line.get("output") {
        Some(Value::String(encoded)) => STANDARD.decode(encoded)?,
        _ => Vec::new(),
    };
    Ok(GoResult::Output(output))
}

fn git(dir: &Path, args: &[&str]) -> String {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into())
}
