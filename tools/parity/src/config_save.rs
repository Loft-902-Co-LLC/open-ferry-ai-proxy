//! Our side of the harness's `config-save/steps` entry: the case's file
//! written to a temporary directory, then each step run on it with
//! `open_ferry_core::config::save` (see `go/parity_config_save.go`).

use std::env;
use std::fs;
use std::iter;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};

use open_ferry_core::config::Config;
use open_ferry_core::config::save::{save_preserving_comments, update_nested_scalar, write_file};
use serde_json::{Value, json};

use crate::cases::Case;
use crate::compare::Deviation;

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

/// Upstream's output as ours would be where upstream wrote a file its own
/// `LoadConfig` refuses (`"unloadable"`, see `go/parity_config_save.go`) and
/// our loader refuses it too: open-ferry checks each file it would write
/// with its loader, so it refuses that write, leaves the file as it was and
/// runs no more steps. That file is dropped, and `"error"` is our loader's
/// message about it, in `LoadConfig`'s wording: `Config::parse` has
/// `ParseConfigBytes`'s, which differs only in the prefix of a syntax or
/// decode error. Left alone when our loader reads the file.
pub fn drop_unloadable(go: &mut Value) -> Option<Deviation> {
    go.get("unloadable")?;
    let mut files = go.get("files")?.as_array()?.clone();
    let message = Config::parse(files.last()?.as_str()?).err()?.to_string();
    let message = match message.strip_prefix("parse config payload: ") {
        Some(rest) => format!("failed to parse config file: {rest}"),
        None => message,
    };
    files.pop();
    *go = json!({ "error": message, "files": files });
    Some(Deviation::UnloadableWrite)
}

/// Upstream's output as ours would be where upstream wrote a comment from a
/// plugin's settings twice, the copies one after the other: the second
/// copy of each such run of comment lines is dropped.
///
/// Upstream replaces `plugins.configs` with the subtree it re-encoded from
/// each plugin's decoded node (`replacePluginConfigsSubtree`), which
/// carries the plugin's comments, some attached to other nodes than in the
/// file; `preserveV8Comments` then puts the file's comment back where it
/// was, so the comment is written twice. open-ferry keeps the file's
/// `plugins.configs`, so it writes the comment once. Only lines that repeat
/// the comment lines before them exactly are dropped, and only comments
/// written once in the case's sources (its file and the files it writes
/// whole; the generator names each comment once), inside a top-level
/// `plugins` section.
pub fn drop_repeated_plugin_comments(case: &Case, go: &mut Value) -> Option<Deviation> {
    let sources: Vec<(&str, Vec<Range<usize>>)> = iter::once(&case.options["file"])
        .chain(
            case.options["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|step| &step["body"]),
        )
        .filter_map(Value::as_str)
        .map(|text| (text, plugin_sections(text)))
        .collect();
    let from_plugins = |line: &str| plugin_comment(&sources, line.trim_start());
    let mut dropped = false;
    for file in go.get_mut("files")?.as_array_mut()? {
        let Some(text) = file.as_str() else {
            continue;
        };
        let lines: Vec<&str> = text.split('\n').collect();
        let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
        let mut index = 0;
        while let Some(rest) = lines.get(index..) {
            if rest.is_empty() {
                break;
            }
            let comments = rest
                .iter()
                .take_while(|line| line.trim_start().starts_with('#'))
                .count();
            let repeated = (1..=comments / 2).find(|&len| {
                let (run, again) = (rest.get(..len), rest.get(len..2 * len));
                run == again && run.is_some_and(|run| run.iter().all(|line| from_plugins(line)))
            });
            let len = repeated.unwrap_or(1);
            kept.extend(rest.iter().take(len));
            index += len + repeated.unwrap_or(0);
        }
        if kept.len() < lines.len() {
            *file = Value::String(kept.join("\n"));
            dropped = true;
        }
    }
    dropped.then_some(Deviation::PluginCommentRepeated)
}

/// The byte ranges of `text`'s top-level `plugins` sections, each from its
/// key's line to the next line at column 0 that isn't a comment.
fn plugin_sections(text: &str) -> Vec<Range<usize>> {
    let mut sections = Vec::new();
    let mut start = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let top_level = !line.starts_with(|c: char| c.is_whitespace() || c == '#');
        if top_level {
            if let Some(from) = start.take() {
                sections.push(from..offset);
            }
            if is_plugins_key(line) {
                start = Some(offset);
            }
        }
        offset += line.len();
    }
    sections.extend(start.map(|from| from..offset));
    sections
}

/// Whether `line` starts the key `plugins`, plain or quoted.
fn is_plugins_key(line: &str) -> bool {
    let quote = line.chars().next().filter(|c| matches!(c, '"' | '\''));
    let rest = quote.map_or(Some(line), |quote| line.strip_prefix(quote));
    let rest = rest.and_then(|rest| rest.strip_prefix("plugins"));
    let rest = match quote {
        Some(quote) => rest.and_then(|rest| rest.strip_prefix(quote)),
        None => rest,
    };
    rest.is_some_and(|rest| rest.trim_start_matches([' ', '\t']).starts_with(':'))
}

/// Whether `comment` (a comment's text, from its `#`) is written once in
/// the sources, at the end of a line, inside a `plugins` section.
fn plugin_comment(sources: &[(&str, Vec<Range<usize>>)], comment: &str) -> bool {
    let mut found = sources.iter().flat_map(|(text, sections)| {
        text.match_indices(comment)
            .filter(|(at, _)| {
                let before = text.get(..*at).unwrap_or_default();
                let after = text.get(at + comment.len()..).unwrap_or_default();
                (before.is_empty() || before.ends_with([' ', '\t', '\n']))
                    && (after.is_empty() || after.starts_with(['\n', '\r']))
            })
            .map(|(at, _)| sections.iter().any(|section| section.contains(&at)))
    });
    found.next() == Some(true) && found.next().is_none()
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::drop_repeated_plugin_comments;
    use crate::cases::Case;
    use crate::compare::Deviation;

    fn case(file: &str, body: &str) -> Case {
        Case::new("plugins", "", "").with_options(json!({
            "file": file,
            "steps": [{ "op": "save", "migrate": false }, { "op": "write", "body": body }],
        }))
    }

    /// Not upstream's: only a repeated run of comments from a `plugins`
    /// section, in the case's file or a file it writes, loses its copy.
    #[test]
    fn drops_only_a_repeated_plugin_comment() {
        let case = case(
            "port: 8317\n\"plugins\":\n  configs:\n    beta:\n      tags: [a] # c1\n#   c2 with words\npprof:\n  # c3\n  enable: true\n",
            "plugins:\n  configs:\n    beta:\n      # c4\n      # c5\n      enabled: true\n",
        );
        let mut go = json!({ "files": [
            "a: 1\n# c1\n# c1\n#   c2 with words\n#   c2 with words\n# c3\n# c3\n",
            "  # c4\n  # c5\n  # c4\n  # c5\n# c4\n  # c4\n",
        ] });
        assert_eq!(
            drop_repeated_plugin_comments(&case, &mut go),
            Some(Deviation::PluginCommentRepeated)
        );
        assert_eq!(
            go,
            json!({ "files": [
                "a: 1\n# c1\n#   c2 with words\n# c3\n# c3\n",
                "  # c4\n  # c5\n# c4\n  # c4\n",
            ] })
        );
        let mut alone = json!({ "files": ["# c1\nport: 1\n# c1\n# c10\n# c10\n"] });
        assert_eq!(drop_repeated_plugin_comments(&case, &mut alone), None);
    }
}
