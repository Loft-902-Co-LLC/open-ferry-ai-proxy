//! Builds and runs the Go harnesses (`go/main.go`, `go/completions/main.go`
//! for the legacy Completions conversions, and `go/interactions/main.go` for
//! the Gemini Interactions translators) inside a CLIProxyAPI checkout.

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
    /// The harness for the Gemini Interactions translators (see
    /// [`is_interactions`]).
    interactions_harness: PathBuf,
}

impl Upstream {
    /// Compiles the harnesses into `work_dir`. They import internal packages,
    /// so they must be compiled as part of the CLIProxyAPI module. An overlay
    /// adds them as `cmd/open-ferry-parity`, `cmd/open-ferry-parity-completions`
    /// and `cmd/open-ferry-parity-interactions` without touching the checkout,
    /// along with `go/openai/export.go`, which exports the Completions
    /// conversions from their package. Each `go/parity_*.go` joins `go/main.go`
    /// in `cmd/open-ferry-parity`, and each `go/interactions/parity_*.go` joins
    /// `go/interactions/main.go`. The Interactions harness is then run once
    /// with no input, as no suite may use it yet.
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
        let main = dir.join("cmd").join("open-ferry-parity");
        let interactions = dir.join("cmd").join("open-ferry-parity-interactions");
        let mut files = Vec::new();
        for (target, source) in [
            (&main, source.clone()),
            (&interactions, source.join("interactions")),
        ] {
            for entry in fs::read_dir(&source)? {
                let name = entry?.file_name().to_string_lossy().into_owned();
                if name.starts_with("parity_") && name.ends_with(".go") {
                    files.push((target.join(&name), source.join(&name)));
                }
            }
        }
        let mut replace = Map::new();
        for (target, source) in files.into_iter().chain([
            (main.join("main.go"), source.join("main.go")),
            (
                interactions.join("main.go"),
                source.join("interactions").join("main.go"),
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
        ]) {
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
        let interactions_harness = work_dir.join(format!(
            "upstream-interactions-harness{}",
            env::consts::EXE_SUFFIX
        ));
        for (binary, package) in [
            (&harness, "./cmd/open-ferry-parity"),
            (&completions_harness, "./cmd/open-ferry-parity-completions"),
            (
                &interactions_harness,
                "./cmd/open-ferry-parity-interactions",
            ),
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
        let output = Command::new(&interactions_harness)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()?;
        if !output.status.success() || !output.stdout.is_empty() {
            return Err(format!(
                "the Interactions harness failed with no input ({})",
                output.status
            )
            .into());
        }

        Ok(Self {
            version: git(&dir, &["describe", "--tags", "--always", "--dirty"]),
            commit: git(&dir, &["rev-parse", "HEAD"]),
            dir,
            harness,
            completions_harness,
            interactions_harness,
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

        // The Completions conversions and the Interactions translators have
        // harnesses of their own (see go/completions/main.go and
        // go/interactions/main.go).
        let harness = if translator.starts_with("completions/") {
            &self.completions_harness
        } else if is_interactions(translator) {
            &self.interactions_harness
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

/// Whether `translator` is an Interactions harness key: one whose package or
/// format, its first or second `/`-separated part, is `interactions`, such as
/// `interactions/claude/request` or `codex/interactions/response`. The
/// `registry/` entries, which may translate to or from Interactions, run in
/// the main harness.
fn is_interactions(translator: &str) -> bool {
    translator
        .split('/')
        .take(2)
        .any(|part| part == "interactions")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactions_keys_have_interactions_as_package_or_format() {
        for key in [
            "interactions/claude/request",
            "claude/interactions/response-non-stream",
            "openai-responses/interactions/request",
            "interactions/interactions/response",
        ] {
            assert!(is_interactions(key), "{key}");
        }
        for key in [
            "registry/request",
            "gemini/openai-chat/request",
            "completions/request",
            "codex/claude/interactions",
        ] {
            assert!(!is_interactions(key), "{key}");
        }
    }
}
