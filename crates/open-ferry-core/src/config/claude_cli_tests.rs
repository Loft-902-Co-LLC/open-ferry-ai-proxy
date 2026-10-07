//! Not upstream's: open-ferry's `claude-cli` list, loaded, checked, saved
//! and edited in both layouts.

use std::fs;
use std::time::Duration;

use super::save::{render_preserving_comments, unmarshal, yaml_path};
use super::testing::TempDir;
use super::v8_edit::{V8Edit, V8Method, edit_v8, validate_v8_config};
use super::{ClaudeCli, ClaudeCliSystemPrompt, ClaudeModel, Config, ConfigErrorKind};

const FULL: &str = "\
claude-cli:
  - name: ' max-1 '
    command: /usr/local/bin/claude
    config-dir: /home/me/.claude-max
    system-prompt: ' Append '
    max-concurrency: 4
    timeout: 90s
    prefix: '/team/'
    priority: 3
    weight: 7
    disabled: true
    models:
      - name: claude-sonnet-5-5
        alias: sonnet
    excluded-models: [' Claude-Haiku-* ']
  - name: max-2
";

#[test]
fn decodes_and_cleans_up_entries() {
    let config = Config::parse(FULL).expect("load");
    let [first, second] = config.claude_cli.as_slice() else {
        panic!("{:?}", config.claude_cli);
    };
    assert_eq!(
        first,
        &ClaudeCli {
            name: "max-1".into(),
            command: "/usr/local/bin/claude".into(),
            config_dir: "/home/me/.claude-max".into(),
            system_prompt: "append".into(),
            max_concurrency: 4,
            timeout: "90s".into(),
            prefix: "team".into(),
            models: vec![ClaudeModel {
                name: "claude-sonnet-5-5".into(),
                alias: "sonnet".into(),
                ..ClaudeModel::default()
            }],
            excluded_models: vec!["claude-haiku-*".into()],
            priority: 3,
            weight: Some(7),
            disabled: true,
        }
    );
    assert_eq!(first.system_prompt_mode(), ClaudeCliSystemPrompt::Append);
    assert_eq!(first.max_concurrency(), 4);
    assert_eq!(first.timeout(), Duration::from_secs(90));

    // The defaults.
    assert_eq!(second.system_prompt_mode(), ClaudeCliSystemPrompt::Replace);
    assert_eq!(second.max_concurrency(), 2);
    assert_eq!(second.timeout(), Duration::from_secs(600));
    assert_eq!(second.weight, None);
    assert!(!second.disabled);
}

#[test]
fn stays_at_the_top_level_of_a_v8_document() {
    let text = format!("config-version: 8\nserver:\n  port: 8317\n{FULL}");
    let config = Config::parse(&text).expect("load");
    assert_eq!(config.port, 8317);
    assert_eq!(config.claude_cli.len(), 2);
    validate_v8_config(text.as_bytes()).expect("a valid v8 config");
}

#[test]
fn rejects_bad_entries_naming_the_key() {
    for (text, message) in [
        (
            "claude-cli: [{command: claude}]\n",
            "claude-cli[0].name: a name is required",
        ),
        (
            "claude-cli: [{name: '  '}]\n",
            "claude-cli[0].name: a name is required",
        ),
        (
            "claude-cli: [{name: a}, {name: A}]\n",
            "claude-cli[1].name: another claude-cli entry has this name",
        ),
        (
            "claude-cli: [{name: a, system-prompt: prepend}]\n",
            "claude-cli[0].system-prompt: must be replace or append",
        ),
        (
            "claude-cli: [{name: a, max-concurrency: -1}]\n",
            "claude-cli[0].max-concurrency: must not be negative",
        ),
        (
            "claude-cli: [{name: a, timeout: soon}]\n",
            "claude-cli[0].timeout: must be a positive duration such as 10m",
        ),
        (
            "claude-cli: [{name: a, timeout: 0s}]\n",
            "claude-cli[0].timeout: must be a positive duration such as 10m",
        ),
        (
            "claude-cli: [{name: a, weight: 1000001}]\n",
            "claude-cli[0].weight: weight must not exceed 1000000",
        ),
        (
            "claude-cli: [{name: a, weight: '5'}]\n",
            "claude-cli[0].weight: weight must be an integer",
        ),
    ] {
        let error = Config::parse(text).expect_err(text);
        assert_eq!(error.kind(), ConfigErrorKind::Invalid, "{text}");
        assert_eq!(error.to_string(), message, "{text}");
    }

    let error = Config::parse("claude-cli: 5\n").expect_err("not a list");
    assert_eq!(error.kind(), ConfigErrorKind::Decode);
    assert!(error.to_string().contains("[]config.ClaudeCLI"), "{error}");
}

#[test]
fn debug_shows_the_entries() {
    let config = Config::parse("claude-cli: [{name: a}]\n").expect("load");
    let debug = format!("{config:?}");
    assert!(
        debug.contains("claude_cli: [ClaudeCli { name: \"a\""),
        "{debug}"
    );
}

/// The file `cfg` saves as over `data`.
fn saved(data: &str, cfg: &Config) -> String {
    let out = render_preserving_comments(data.as_bytes(), cfg, false).expect("save");
    String::from_utf8(out).expect("utf-8")
}

#[test]
fn saves_and_round_trips_in_the_legacy_layout() {
    let data = format!("# head\nport: 8317\n{FULL}");
    let mut cfg = Config::parse(&data).expect("load");
    cfg.claude_cli[1].timeout = "5m".into();
    cfg.claude_cli.push(ClaudeCli {
        name: "max-3".into(),
        config_dir: "/srv/claude-3".into(),
        ..ClaudeCli::default()
    });
    let out = saved(&data, &cfg);
    assert!(out.starts_with("# head\n"), "{out}");
    let reloaded = Config::parse(&out).expect("reload");
    assert_eq!(reloaded.claude_cli, cfg.claude_cli, "{out}");
    // Defaults aren't written out.
    let root = unmarshal(out.as_bytes()).expect("yaml");
    let root = root.content.first().expect("root");
    let third = yaml_path(root, "claude-cli").expect("list").content[2].clone();
    let keys: Vec<&str> = third
        .content
        .iter()
        .step_by(2)
        .map(|key| key.value.as_str())
        .collect();
    assert_eq!(keys, ["name", "config-dir"], "{out}");

    // Emptying the list empties the file's.
    cfg.claude_cli.clear();
    let out = saved(&data, &cfg);
    assert!(
        Config::parse(&out).expect("reload").claude_cli.is_empty(),
        "{out}"
    );

    // A file without the list doesn't gain one.
    let plain = "port: 8317\n";
    let cfg = Config::parse(plain).expect("load");
    let out = saved(plain, &cfg);
    assert!(!out.contains("claude-cli"), "{out}");
}

#[test]
fn saves_and_round_trips_in_the_v8_layout() {
    let data = format!("config-version: 8\nserver:\n  port: 8317\n{FULL}");
    let mut cfg = Config::parse(&data).expect("load");
    cfg.claude_cli[0].max_concurrency = 1;
    let out = saved(&data, &cfg);
    validate_v8_config(out.as_bytes()).expect("a valid v8 config");
    assert!(
        !out.contains("# claude-cli"),
        "kept, not commented out:\n{out}"
    );
    let reloaded = Config::parse(&out).expect("reload");
    assert_eq!(reloaded.claude_cli, cfg.claude_cli, "{out}");

    // The list alone doesn't make a legacy file v8.
    let legacy = "port: 8317\nclaude-cli: [{name: a}]\n";
    let cfg = Config::parse(legacy).expect("load");
    let out = saved(legacy, &cfg);
    assert!(out.contains("port: 8317"), "stays legacy:\n{out}");
}

#[test]
fn v8_edits_set_and_check_the_list() {
    let dir = TempDir::new();
    let path = dir.write("config.yaml", "config-version: 8\nserver:\n  port: 8317\n");
    let put = V8Edit {
        method: V8Method::Put,
        path: vec!["claude-cli".into()],
        body: br#"[{"name":"max-1","config-dir":"/srv/claude","timeout":"2m"}]"#.to_vec(),
        yaml: false,
    };
    let config = edit_v8(&path, &put).expect("edit");
    assert_eq!(config.claude_cli.len(), 1);
    assert_eq!(config.claude_cli[0].timeout(), Duration::from_secs(120));
    let text = fs::read_to_string(&path).expect("read");
    assert!(text.contains("claude-cli:"), "{text}");
    assert_eq!(
        Config::load(&path).expect("load").claude_cli,
        config.claude_cli
    );

    // An unknown key in an entry is refused, as for the other lists.
    let unknown = V8Edit {
        body: br#"[{"name":"max-1","api-key":"x"}]"#.to_vec(),
        ..put.clone()
    };
    let error = edit_v8(&path, &unknown).expect_err("unknown key");
    assert!(error.to_string().contains("api-key"), "{error}");

    // So is an entry the loader refuses.
    let unnamed = V8Edit {
        body: br#"[{"command":"claude"}]"#.to_vec(),
        ..put
    };
    let error = edit_v8(&path, &unnamed).expect_err("no name");
    assert!(error.to_string().contains("claude-cli[0].name"), "{error}");
}

#[test]
fn reload_lines_name_each_change() {
    let old = Config::parse("claude-cli: [{name: a, timeout: 5m}]\n").expect("load");
    let new = Config::parse(
        "claude-cli: [{name: a, timeout: 10m, disabled: true, weight: 2, models: [{name: m}]}]\n",
    )
    .expect("load");
    let lines = super::diff::build_change_details(&old, &new);
    assert_eq!(
        lines,
        [
            "claude-cli[0].timeout: 5m -> 10m",
            "claude-cli[0].weight: <unset> -> 2",
            "claude-cli[0].disabled: false -> true",
            "claude-cli[0].models: updated (0 -> 1 entries)",
        ]
    );
    let more = Config::parse("claude-cli: [{name: a}, {name: b}]\n").expect("load");
    assert_eq!(
        super::diff::build_change_details(&old, &more),
        ["claude-cli count: 1 -> 2"]
    );
}
