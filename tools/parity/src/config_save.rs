//! Our side of the harness's `config-save/steps` entry: the case's file
//! written to a temporary directory, then each step run on it with
//! `open_ferry_core::config::save` (see `go/parity_config_save.go`).

pub mod counterfactual;
pub mod folded;

use std::env;
use std::fs;
use std::iter;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};

use open_ferry_core::config::save::{save_preserving_comments, update_nested_scalar, write_file};
use open_ferry_core::config::{AnyValue, Config};
use serde_json::{Value, json};

use crate::cases::Case;
use crate::compare::Deviation;

pub(crate) const LOSSY_PLUGIN_FIXTURES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/open-ferry-core/src/config/testdata/plugin-lossy-reencode-save.json"
));

/// Recognize only the proven folded-scalar loss: Go makes a source plugin
/// string empty and appends its value to the next comment. Rust must retain
/// the complete source plugin value, each source comment with its alias-expanded multiplicity,
/// and every other output byte. Comment-only losses are not accepted here.
/// An identical whole-file write starts a new source; differences during
/// that write itself are never accepted by this rule.
pub fn repair_lossy_plugin_reencode(
    case: &Case,
    go: &mut Value,
    rust: &Value,
) -> Option<Deviation> {
    if go == rust {
        return None;
    }
    let mut source = case.options["file"].as_str()?;
    let steps = case.options["steps"].as_array()?;
    if !steps.iter().all(|step| match step["op"].as_str() {
        Some("save" | "write") => true,
        Some("nested") => step["keys"].as_array().is_some_and(|keys| {
            !keys.is_empty() && keys[0].as_str().is_some_and(|key| key != "plugins")
        }),
        _ => false,
    }) || go.as_object()?.len() != 1
        || rust.as_object()?.len() != 1
    {
        return None;
    }
    let go_files = go["files"].as_array()?;
    let rust_files = rust["files"].as_array()?;
    if go_files.len() != steps.len() || rust_files.len() != steps.len() {
        return None;
    }
    let mut repaired = go.clone();
    let mut losses = 0;
    for (index, (go, rust)) in go_files.iter().zip(rust_files).enumerate() {
        let (go, rust) = (go.as_str()?, rust.as_str()?);
        if steps[index]["op"] == "write" {
            if go != rust {
                return None;
            }
            source = steps[index]["body"].as_str()?;
        }
        let original = plugin_values(source)?;
        let rust_values = plugin_values(rust)?;
        if rust_values != original {
            return None;
        }
        if go == rust {
            continue;
        }
        let (PluginValues::Mapping(go_value), PluginValues::Mapping(rust_value)) =
            (plugin_values(go)?, rust_values)
        else {
            return None;
        };
        let sources = [(source, vec![configs_range(source)?])];
        let expected = scalar_losses(&go_value, &rust_value)?;
        let (go_range, rust_range) = (configs_range(go)?, configs_range(rust)?);
        let go_lines: Vec<&str> = go.split_inclusive('\n').collect();
        let rust_lines: Vec<&str> = rust.split_inclusive('\n').collect();
        if go_lines.len() != rust_lines.len() {
            return None;
        }
        let (mut go_at, mut rust_at, mut line, mut found) = (0, 0, 0, 0);
        let mut output = String::new();
        while line < go_lines.len() {
            let (g, r) = (go_lines[line], rust_lines[line]);
            if g == r {
                output.push_str(g);
                go_at += g.len();
                rust_at += r.len();
                line += 1;
                continue;
            }
            if !go_range.contains(&go_at) || !rust_range.contains(&rust_at) {
                return None;
            }
            let (g_next, r_next) = (*go_lines.get(line + 1)?, *rust_lines.get(line + 1)?);
            let r = r.strip_suffix('\n')?;
            let indent = r.len() - r.trim_start_matches(' ').len();
            let scalar = r.get(indent..)?.strip_prefix("- ")?;
            let mut reader = serde_json::Deserializer::from_str(scalar).into_iter::<String>();
            let value = reader.next()?.ok()?;
            if value.is_empty() || value.trim() != value || value.contains(['\n', '\r']) {
                return None;
            }
            let comment = scalar.get(reader.byte_offset()..)?.strip_prefix(' ')?;
            let next_comment = r_next.strip_suffix('\n')?.trim_start_matches(' ');
            if !comment.starts_with('#')
                || !next_comment.starts_with('#')
                || ![comment, next_comment].into_iter().all(|comment| {
                    plugin_comment(&sources, comment)
                        && open_ferry_core::config::save::parity::expanded_plugin_comment_count(
                            source, comment,
                        )
                        .is_some_and(|expected| {
                            expected > 0 && comment_occurrences(rust, comment) == expected
                        })
                })
            {
                return None;
            }
            let prefix = r.get(..indent)?;
            if g != format!("{prefix}- >- {comment}\n")
                && g != format!("{prefix}- \"\" {comment}\n")
            {
                return None;
            }
            if g_next != format!("{} {value}\n", r_next.strip_suffix('\n')?) {
                return None;
            }
            output.push_str(rust_lines[line]);
            output.push_str(r_next);
            go_at += g.len() + g_next.len();
            rust_at += rust_lines[line].len() + r_next.len();
            line += 2;
            found += 1;
        }
        if found != expected || output != rust {
            return None;
        }
        losses += found;
        repaired["files"][index] = Value::String(output);
    }
    if losses == 0 {
        return None;
    }
    *go = repaired;
    Some(Deviation::UpstreamLossyPluginReencode)
}

#[derive(PartialEq)]
enum PluginValues {
    Missing,
    Mapping(AnyValue),
}

fn plugin_values(text: &str) -> Option<PluginValues> {
    let AnyValue::Map(root) = AnyValue::parse_yaml(text).ok()? else {
        return None;
    };
    let Some(plugins) = root.get("plugins") else {
        return Some(PluginValues::Missing);
    };
    let AnyValue::Map(plugins) = plugins else {
        return None;
    };
    let Some(value) = plugins.get("configs") else {
        return Some(PluginValues::Missing);
    };
    fn supported(value: &AnyValue) -> bool {
        match value {
            AnyValue::AnyMap => false,
            AnyValue::Map(map) => map.values().all(supported),
            AnyValue::Seq(seq) => seq.iter().all(supported),
            _ => true,
        }
    }
    (matches!(value, AnyValue::Map(_)) && supported(value))
        .then(|| PluginValues::Mapping(value.clone()))
}

fn scalar_losses(go: &AnyValue, rust: &AnyValue) -> Option<usize> {
    if go == rust {
        return Some(0);
    }
    match (go, rust) {
        (AnyValue::Null, AnyValue::Str(value)) if !value.is_empty() => Some(1),
        (AnyValue::Str(empty), AnyValue::Str(value)) if empty.is_empty() && !value.is_empty() => {
            Some(1)
        }
        (AnyValue::Map(go), AnyValue::Map(rust)) if go.keys().eq(rust.keys()) => go
            .values()
            .zip(rust.values())
            .map(|(g, r)| scalar_losses(g, r))
            .sum(),
        (AnyValue::Seq(go), AnyValue::Seq(rust)) if go.len() == rust.len() => {
            go.iter().zip(rust).map(|(g, r)| scalar_losses(g, r)).sum()
        }
        _ => None,
    }
}

fn comment_occurrences(text: &str, comment: &str) -> usize {
    text.match_indices(comment)
        .filter(|(at, _)| {
            let before = &text[..*at];
            let after = &text[at + comment.len()..];
            (before.is_empty() || before.ends_with([' ', '\t', '\n']))
                && (after.is_empty() || after.starts_with(['\n', '\r']))
        })
        .count()
}

/// Conservative byte span of a block `plugins.configs` value. Ambiguous,
/// duplicate, flow and unusual key presentations are left Different.
fn configs_range(text: &str) -> Option<Range<usize>> {
    let sections = plugin_sections(text);
    let [plugins] = sections.as_slice() else {
        return None;
    };
    let mut offset = plugins.start;
    let lines: Vec<_> = text[plugins.clone()]
        .split_inclusive('\n')
        .map(|line| {
            let at = offset;
            offset += line.len();
            (at, line)
        })
        .collect();
    let indent = lines
        .iter()
        .skip(1)
        .filter_map(|(_, line)| {
            let trimmed = line.trim_start_matches(' ');
            (!trimmed.trim().is_empty() && !trimmed.starts_with('#'))
                .then_some(line.len() - trimmed.len())
        })
        .min()?;
    if indent == 0 {
        return None;
    }
    let mut span = None;
    for (at, line) in lines.into_iter().skip(1) {
        let trimmed = line.trim_start_matches(' ');
        if trimmed.trim().is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let depth = line.len() - trimmed.len();
        if let Some(start) = span
            && depth <= indent
        {
            return Some(start..at);
        }
        if depth == indent {
            for key in ["configs:", "\"configs\":", "'configs':"] {
                if let Some(rest) = trimmed.strip_prefix(key) {
                    if !rest.trim().is_empty() && !rest.trim_start().starts_with('#') {
                        return None;
                    }
                    span = Some(at + line.len());
                }
            }
        }
    }
    span.map(|start| start..plugins.end)
}

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
/// `plugins` section. Archived lines may carry one extra `# ` prefix;
/// the repeated runs must still match byte for byte, including that prefix.
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
    let from_plugins = |line: &str| {
        let line = line.trim_start();
        plugin_comment(&sources, line)
            || line.strip_prefix("# ").is_some_and(|comment| {
                comment.starts_with('#') && plugin_comment(&sources, comment)
            })
    };
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

    #[test]
    fn upstream_lossy_plugin_reencode_requires_the_complete_saved_outputs() {
        let mut fixtures: Vec<serde_json::Value> =
            serde_json::from_str(super::LOSSY_PLUGIN_FIXTURES).unwrap();
        fixtures.push(
            serde_json::from_str(include_str!(
                "cases/testdata/config-save-plugin-loss-after-write.json"
            ))
            .unwrap(),
        );
        for fixture in fixtures {
            let case = Case::new("lossy-plugin", "", "").with_options(fixture["options"].clone());
            let mut go = fixture["upstream_output"].clone();
            let rust = &fixture["open_ferry_output"];
            assert_ne!(&go, rust);
            assert_eq!(
                super::repair_lossy_plugin_reencode(&case, &mut go, rust),
                Some(Deviation::UpstreamLossyPluginReencode)
            );
            assert_eq!(&go, rust);
        }
    }

    #[test]
    fn alias_shared_plugin_loss_requires_every_expanded_comment_copy() {
        let f: serde_json::Value = serde_json::from_str(include_str!(
            "cases/testdata/config-save-alias-plugin-loss.json"
        ))
        .unwrap();
        let case = Case::new("alias-loss", "", "").with_options(f["options"].clone());
        let mut go = f["upstream_output"].clone();
        super::drop_repeated_plugin_comments(&case, &mut go);
        let rust = &f["open_ferry_output"];
        assert_eq!(
            crate::translator::Translator::ConfigSave
                .run_rust(&case)
                .unwrap(),
            *rust
        );
        let mut accepted = go.clone();
        assert_eq!(
            super::repair_lossy_plugin_reencode(&case, &mut accepted, rust),
            Some(Deviation::UpstreamLossyPluginReencode)
        );
        assert_eq!(&accepted, rust);
        for extra in [false, true] {
            let mut changed = rust.clone();
            let first = changed["files"][0].as_str().unwrap();
            let comment = "# c32";
            changed["files"][0] = if extra {
                format!("{first}{comment}\n")
            } else {
                first.replacen(comment, "# changed", 1)
            }
            .into();
            let mut original = go.clone();
            assert!(super::repair_lossy_plugin_reencode(&case, &mut original, &changed).is_none());
        }
    }

    #[test]
    fn upstream_lossy_plugin_reencode_keeps_near_misses_different() {
        let source = "plugins:\n  configs:\n    alpha:\n      tags: &s # moved\n        - >- # own\n          a\ndebug: false\n";
        let case =
            Case::new("lossy", "", "").with_options(json!({"file":source,"steps":[{"op":"save"}]}));
        let go = "plugins:\n  configs:\n    alpha:\n      tags:\n        - >- # moved\n# own a\ndebug: false\n";
        let rust = "plugins:\n  configs:\n    alpha:\n      tags:\n        - \"a\" # moved\n# own\ndebug: false\n";
        let mut accepted = json!({"files":[go]});
        assert_eq!(
            super::repair_lossy_plugin_reencode(&case, &mut accepted, &json!({"files":[rust]})),
            Some(Deviation::UpstreamLossyPluginReencode)
        );
        for (g, r) in [
            (go.replace("debug: false", "debug: true"), rust.to_owned()),
            (go.replace("# own a", "# different a"), rust.to_owned()),
            (go.replace("# own a", "# own b"), rust.to_owned()),
            (go.replace("- >-", "- \"z\""), rust.to_owned()),
            (go.to_owned(), rust.replace("\"a\"", "\"b\"")),
            (
                go.replace("# own a", "# own b"),
                rust.replace("\"a\"", "\"b\""),
            ),
            (go.to_owned(), rust.replace("# own\n", "")),
            (go.to_owned(), rust.replace("# own\n", "# own\n# own\n")),
            (go.replace("# own a", " # own a"), rust.to_owned()),
            (go.to_owned() + "# outside changed\n", rust.to_owned()),
        ] {
            let mut output = json!({"files":[g]});
            let before = output.clone();
            assert_eq!(
                super::repair_lossy_plugin_reencode(&case, &mut output, &json!({"files":[r]})),
                None
            );
            assert_eq!(output, before);
        }
        for options in [
            json!({"file":source.replace("          a", "          b"),"steps":[{"op":"save"}]}),
            json!({"file":source,"steps":[{"op":"nested","keys":["plugins","configs"]}]}),
            json!({"file":source,"steps":[{"op":"write","body":source}]}),
        ] {
            let case = Case::new("near-miss", "", "").with_options(options);
            assert_eq!(
                super::repair_lossy_plugin_reencode(
                    &case,
                    &mut json!({"files":[go]}),
                    &json!({"files":[rust]})
                ),
                None
            );
        }
        // A similar loss outside configs is not accepted even when both
        // comments originated in plugins and the plugin values are retained.
        let kept = "plugins:\n  configs:\n    alpha:\n      tags: [a]\npayload:\n  tags:\n";
        assert_eq!(
            super::repair_lossy_plugin_reencode(
                &case,
                &mut json!({"files":[format!("{kept}    - >- # moved\n# own a\n")]}),
                &json!({"files":[format!("{kept}    - \"a\" # moved\n# own\n")]})
            ),
            None
        );
        let mut comment_only = json!({"files":[rust.replace("# own\n", "")]});
        assert_eq!(
            super::repair_lossy_plugin_reencode(&case, &mut comment_only, &json!({"files":[rust]})),
            None
        );
        let case = Case::new("lost-before-later-save", "", "")
            .with_options(json!({"file":source,"steps":[{"op":"save"},{"op":"save"}]}));
        assert_eq!(
            super::repair_lossy_plugin_reencode(
                &case,
                &mut json!({"files":[go,go]}),
                &json!({"files":[go,rust]})
            ),
            None
        );
    }

    #[test]
    fn upstream_lossy_plugin_reencode_tracks_only_identical_write_sources() {
        let source = "plugins:\n  configs:\n    alpha:\n      tags: &s # moved\n        - >- # own\n          a\n";
        let go = "plugins:\n  configs:\n    alpha:\n      tags:\n        - >- # moved\n# own a\n";
        let rust =
            "plugins:\n  configs:\n    alpha:\n      tags:\n        - \"a\" # moved\n# own\n";
        let options = json!({"file":source.replace("          a", "          b"),"steps":[{"op":"write","body":source},{"op":"save"}]});
        let case = Case::new("write-source", "", "").with_options(options.clone());
        let mut output = json!({"files":[source,go]});
        let expected = json!({"files":[source,rust]});
        assert_eq!(
            super::repair_lossy_plugin_reencode(&case, &mut output, &expected),
            Some(Deviation::UpstreamLossyPluginReencode)
        );
        assert_eq!(output, expected);
        let mut changed = options.clone();
        changed["steps"][0]["body"] = source.replace("          a", "          b").into();
        assert_eq!(
            super::repair_lossy_plugin_reencode(
                &Case::new("wrong-body", "", "").with_options(changed),
                &mut json!({"files":[source,go]}),
                &expected
            ),
            None
        );
        assert_eq!(
            super::repair_lossy_plugin_reencode(
                &case,
                &mut json!({"files":[source.replace("# own", "# changed"),go]}),
                &expected
            ),
            None
        );
    }

    fn case(file: &str, body: &str) -> Case {
        Case::new("plugins", "", "").with_options(json!({
            "file": file,
            "steps": [{ "op": "save", "migrate": false }, { "op": "write", "body": body }],
        }))
    }

    #[test]
    fn archived_plugin_copies_match_the_saved_seed9_outputs() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "cases/testdata/config-save-archived-plugin-comment.json"
        ))
        .unwrap();
        let case = Case::new("seed9", "", "").with_options(fixture["options"].clone());
        let mut go = fixture["upstream_output"].clone();
        let rust = &fixture["open_ferry_output"];
        assert!(
            !crate::compare::compare(&go, rust, &[])
                .differences
                .is_empty()
        );
        assert_eq!(
            drop_repeated_plugin_comments(&case, &mut go),
            Some(Deviation::PluginCommentRepeated)
        );
        assert!(
            crate::compare::compare(&go, rust, &[])
                .differences
                .is_empty()
        );
    }

    #[test]
    fn archived_plugin_rule_preserves_different_prefixes_and_unrelated_comments() {
        let case = case(
            "plugins:\n  configs:\n    alpha:\n      tags: [a]\n      # source comment\n",
            "",
        );
        for text in [
            "# # source comment\n# source comment\n",
            "# # source comment\n # # source comment\n",
            "# # unrelated comment\n# # unrelated comment\n",
        ] {
            let mut go = json!({"files": [text]});
            let rust = json!({"files": ["# # source comment\n"]});
            assert_eq!(drop_repeated_plugin_comments(&case, &mut go), None);
            assert!(
                !crate::compare::compare(&go, &rust, &[])
                    .differences
                    .is_empty()
            );
        }
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
