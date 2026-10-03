//! Builds and runs the Go harnesses (`go/main.go`, and `go/completions/main.go`
//! for the legacy Completions conversions) inside a CLIProxyAPI checkout.

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
    /// The harness for the `completions/` translators.
    completions_harness: PathBuf,
}

impl Upstream {
    /// Compiles the harnesses into `work_dir`. They import internal packages,
    /// so they must be compiled as part of the CLIProxyAPI module. An overlay
    /// adds them as `cmd/open-ferry-parity` and `cmd/open-ferry-parity-completions`
    /// without touching the checkout, along with `go/openai/export.go`, which
    /// exports the Completions conversions from their package.
    pub fn build(dir: &Path, go: &Path, work_dir: &Path) -> Result<Self, Box<dyn Error>> {
        let dir = std::path::absolute(dir)?;
        if !dir.join("go.mod").is_file() {
            return Err(format!(
                "{} has no go.mod; pass a CLIProxyAPI checkout",
                dir.display()
            )
            .into());
        }
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("go");
        let handlers = dir.join("sdk").join("api").join("handlers").join("openai");
        let mut replace = Map::new();
        for (target, source) in [
            (
                dir.join("cmd").join("open-ferry-parity").join("main.go"),
                source.join("main.go"),
            ),
            (
                dir.join("cmd")
                    .join("open-ferry-parity-completions")
                    .join("main.go"),
                source.join("completions").join("main.go"),
            ),
            (
                handlers.join("zz_open_ferry_parity_export.go"),
                source.join("openai").join("export.go"),
            ),
        ] {
            replace.insert(
                target.to_string_lossy().into_owned(),
                source.to_string_lossy().into_owned().into(),
            );
        }
        let overlay = work_dir.join("overlay.json");
        fs::write(&overlay, json!({ "Replace": replace }).to_string())?;

        let harness = work_dir.join(format!("upstream-harness{}", env::consts::EXE_SUFFIX));
        let completions_harness = work_dir.join(format!(
            "upstream-completions-harness{}",
            env::consts::EXE_SUFFIX
        ));
        for (binary, package) in [
            (&harness, "./cmd/open-ferry-parity"),
            (&completions_harness, "./cmd/open-ferry-parity-completions"),
        ] {
            let status = Command::new(go)
                .current_dir(&dir)
                .arg("build")
                .arg("-overlay")
                .arg(&overlay)
                .arg("-o")
                .arg(binary)
                .arg(package)
                .status()
                .map_err(|err| {
                    format!(
                        "could not run {}: {err} (install Go or pass --go)",
                        go.display()
                    )
                })?;
            if !status.success() {
                return Err(format!("go build of {package} failed ({status})").into());
            }
        }

        Ok(Self {
            version: git(&dir, &["describe", "--tags", "--always", "--dirty"]),
            commit: git(&dir, &["rev-parse", "HEAD"]),
            dir,
            harness,
            completions_harness,
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
            let mut line =
                json!({ "translator": translator, "model": case.model, "request": case.request });
            if !case.translated_request.is_empty() {
                line["translated_request"] = json!(case.translated_request);
            }
            if !case.events.is_empty() {
                line["events"] = json!(case.events);
            }
            if !case.options.is_null() {
                line["options"] = case.options.clone();
            }
            serde_json::to_writer(&mut input, &line)?;
            input.write_all(b"\n")?;
        }
        input
            .into_inner()
            .map_err(|err| err.into_error())?
            .sync_all()?;

        // The Completions conversions have a harness of their own (see
        // go/completions/main.go).
        let harness = if translator.starts_with("completions/") {
            &self.completions_harness
        } else {
            &self.harness
        };
        let output = Command::new(harness)
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
