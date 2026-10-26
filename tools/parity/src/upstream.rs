//! Builds and runs the Go harnesses (`go/main.go`, `go/completions/main.go`
//! for the legacy Completions conversions, `go/interactions/main.go` for
//! the Gemini Interactions translators, and `go/helps/main.go` for the
//! executor helpers) inside a CLIProxyAPI checkout.

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
    /// `git describe` output for the checkout, such as `v8.0.38`.
    pub version: String,
    pub commit: String,
    harness: PathBuf,
    /// The harness for the `completions/` translators.
    completions_harness: PathBuf,
    /// The harness for the Gemini Interactions translators (see
    /// [`is_interactions`]).
    interactions_harness: PathBuf,
    /// The harness for the executor helpers (see [`is_helps`]).
    helps_harness: PathBuf,
}

impl Upstream {
    /// Compiles the harnesses into `work_dir`. They import internal packages,
    /// so they must be compiled as part of the CLIProxyAPI module. An overlay
    /// adds them as `cmd/open-ferry-parity`, `cmd/open-ferry-parity-completions`,
    /// `cmd/open-ferry-parity-interactions` and `cmd/open-ferry-parity-helps`
    /// without touching the checkout, along with `go/openai/export.go`, which
    /// exports the Completions conversions from their package. Each
    /// `go/parity_*.go` joins `go/main.go` in `cmd/open-ferry-parity`, and
    /// each `go/interactions/parity_*.go` and `go/helps/parity_*.go` joins
    /// the `main.go` beside it. The Interactions harness is then run once
    /// with no input, as no suite may use it yet.
    ///
    /// The builds use a Go build cache of `work_dir`'s own, `go-build`.
    /// Every checkout overlays the same paths in the CLIProxyAPI module with
    /// its own files, and with the shared cache, parity runs in two
    /// checkouts at once got harnesses with the other's files, or input
    /// they couldn't read.
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
        let helps = dir.join("cmd").join("open-ferry-parity-helps");
        let mut files = Vec::new();
        for (target, source) in [
            (&main, source.clone()),
            (&interactions, source.join("interactions")),
            (&helps, source.join("helps")),
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
            (helps.join("main.go"), source.join("helps").join("main.go")),
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
        // Observe exact rejected intermediate inputs without changing either
        // yaml.v3 or the read-only snapshot. Each wrapper calls the original
        // Unmarshal once and returns its error unchanged.
        for name in ["config_yaml.go", "config_v8.go"] {
            let original_path = dir.join("internal/config").join(name);
            let original = fs::read_to_string(&original_path)?;
            let mut traced = original.replace("yaml.Unmarshal(", "parityUnmarshal(");
            if name == "config_v8.go" {
                let needle = "comments = append(comments, \"# \"+strings.ReplaceAll(text, \"\\n\", \"\\n# \"))";
                if traced.matches(needle).count() != 1 {
                    return Err("ownership oracle: pinned archive construction changed".into());
                }
                traced = traced.replace(needle, &format!("{needle}\n\t\tparityArchiveEntry(node.Content[i], node.Content[i+1], section, comments[len(comments)-1])"));
                let footer = "archive.FootComment = strings.TrimSpace(archive.FootComment + \"\\n\" + strings.Join(comments, \"\\n\"))";
                if traced.matches(footer).count() != 1 {
                    return Err("ownership oracle: pinned archive append changed".into());
                }
                traced = traced.replace(
                    footer,
                    &format!("parityArchiveAppended(archive.FootComment)\n\t\t{footer}"),
                );
            }
            let copy = std::path::absolute(work_dir.join(format!("trace-{name}")))?;
            fs::write(&copy, traced)?;
            replace.insert(
                original_path.to_string_lossy().into_owned(),
                copy.to_string_lossy().into_owned().into(),
            );
        }
        // The ownership oracle calls the pin's tree edits before emission.
        // Extract operation bodies rather than maintaining a second migration
        // algorithm. Exact anchors deliberately fail closed on upstream drift.
        let tree_file = std::path::absolute(work_dir.join("ownership-tree.go"))?;
        fs::write(&tree_file, ownership_tree(&dir)?)?;
        replace.insert(
            dir.join("internal/config/zz_parity_ownership_tree.go")
                .to_string_lossy()
                .into_owned(),
            tree_file.to_string_lossy().into_owned().into(),
        );
        for name in [
            "ownership.go",
            "ownership_test.go",
            "archives.go",
            "archives_test.go",
        ] {
            replace.insert(
                dir.join("internal/config")
                    .join(format!("zz_parity_{name}"))
                    .to_string_lossy()
                    .into_owned(),
                source
                    .join("config")
                    .join(name)
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            );
        }
        let trace_file = std::path::absolute(work_dir.join("trace-unmarshal.go"))?;
        fs::write(&trace_file, include_str!("../go/config/trace.go"))?;
        replace.insert(
            dir.join("internal/config/zz_parity_trace.go")
                .to_string_lossy()
                .into_owned(),
            trace_file.to_string_lossy().into_owned().into(),
        );
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
        let helps_harness =
            work_dir.join(format!("upstream-helps-harness{}", env::consts::EXE_SUFFIX));
        for (binary, package) in [
            (&harness, "./cmd/open-ferry-parity"),
            (&completions_harness, "./cmd/open-ferry-parity-completions"),
            (
                &interactions_harness,
                "./cmd/open-ferry-parity-interactions",
            ),
            (&helps_harness, "./cmd/open-ferry-parity-helps"),
        ] {
            let status = Command::new(go)
                .current_dir(&dir)
                .env("GOCACHE", work_dir.join("go-build"))
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
            helps_harness,
        })
    }

    /// Runs `translator` on every case. Results are in case order.
    pub fn run(
        &self,
        translator: &str,
        cases: &[Case],
        work_dir: &Path,
    ) -> Result<Vec<GoResult>, Box<dyn Error>> {
        let _timing = crate::config_save::metrics::Timer::start(1);
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

        // The Completions conversions, the Interactions translators and the
        // executor helpers have harnesses of their own (see
        // go/completions/main.go, go/interactions/main.go and
        // go/helps/main.go).
        let harness = if translator.starts_with("completions/") {
            &self.completions_harness
        } else if is_interactions(translator) {
            &self.interactions_harness
        } else if is_helps(translator) {
            &self.helps_harness
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

fn ownership_tree(dir: &Path) -> Result<String, Box<dyn Error>> {
    fn between<'a>(text: &'a str, start: &str, end: &str) -> Result<&'a str, Box<dyn Error>> {
        if text.matches(start).count() != 1 || text.matches(end).count() != 1 {
            return Err("ownership oracle: pinned tree-edit anchors changed".into());
        }
        let tail = text.split_once(start).unwrap().1;
        Ok(tail
            .split_once(end)
            .ok_or("ownership oracle: reversed anchors")?
            .0)
    }
    let yaml =
        fs::read_to_string(dir.join("internal/config/config_yaml.go"))?.replace("\r\n", "\n");
    let v8 = fs::read_to_string(dir.join("internal/config/config_v8.go"))?.replace("\r\n", "\n");
    let save = between(
        &yaml,
        "\tlayout := deepCopyNode(original.Content[0])",
        "\tNormalizeCollectionNodeStyles(original.Content[0])",
    )?;
    let save = format!("\tlayout := deepCopyNode(root){save}")
        .replace("original.Content[0]", "root")
        .replace("(*legacyConfig)(persistCfg)", "(*legacyConfig)(cfg)")
        .replace("return err", "return nil, err")
        .replace("return fmt.Errorf", "return nil, fmt.Errorf")
        .replace(
            "\t// Keep obsolete roots",
            "\tif roundtrip { parityClearIdentities(&generated) } else { parityClearGenerated(&generated) }\n\t// Keep obsolete roots",
        );
    let normalize = between(
        &v8,
        "\tif _, err := flattenV8(root); err != nil {",
        "// IsV8ConfigLayout identifies",
    )?;
    let normalize = format!("\tif _, err := flattenV8(root); err != nil {{{normalize}")
        .replace("\tdoc.Content[0] = root\n", "")
        .replace("return nil, false, err", "return nil, err")
        .replace("return data, false, nil", "return root, nil")
        .replace(
            "\tout, err := yaml.Marshal(&doc)\n\treturn out, true, err",
            "\treturn root, nil",
        );
    Ok(format!(
        "package config\nimport (\"fmt\"; \"gopkg.in/yaml.v3\")\nfunc paritySaveTree(root *yaml.Node, cfg *Config, data []byte, migrating bool, roundtrip bool)(*yaml.Node,error){{\n{save}\nNormalizeCollectionNodeStyles(root)\nif migrating {{ if roundtrip {{ root,err = parityRoundTrip(root,data); if err != nil {{return nil,err}} }}; return parityNormalizeTree(root,true) }}\nreturn root,nil\n}}\nfunc parityNormalizeTree(root *yaml.Node,migrate bool)(*yaml.Node,error){{\n{normalize}"
    ))
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

/// The packages of the executor helpers' harness keys.
const HELPS_PACKAGES: &[&str] = &["payload", "usage", "ttft"];

/// Whether `translator` is an executor helpers' harness key: one whose
/// package, its first `/`-separated part, is in [`HELPS_PACKAGES`], such as
/// `payload/apply`, `usage/parse` or `ttft/token-event`. Importing upstream's `helps` package registers
/// translators the main harness's `registry/` entries must not see (see
/// `go/helps/main.go`), so an entry that imports it is in that harness and
/// its package is listed here.
fn is_helps(translator: &str) -> bool {
    translator
        .split('/')
        .next()
        .is_some_and(|package| HELPS_PACKAGES.contains(&package))
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

    #[test]
    fn helps_keys_have_a_helpers_package() {
        for key in ["payload/apply", "usage/parse", "ttft/token-event"] {
            assert!(is_helps(key), "{key}");
        }
        for key in [
            "registry/request",
            "codex/payload/request",
            "payloads/apply",
            "interactions/claude/request",
        ] {
            assert!(!is_helps(key), "{key}");
        }
    }
}
