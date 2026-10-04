//! Not upstream's: tests of the gjson and sjson ports, path resolution,
//! model matching, rule compilation and the choice of rules a call reads.

use std::sync::Arc;

use open_ferry_core::config::Config;
use serde_json::Value;

use super::{Args, json, rules};
use crate::payload::gjson::{self, match_limit};
use crate::payload::sjson::{self, SetError};
use crate::payload::{Rules, Selected, matchers, path, select};

fn get(doc: &str, path: &str) -> Option<Value> {
    gjson::get(&json(doc), path).map(|found| found.to_value())
}

#[test]
fn gjson_reads_keys_indexes_counts_and_queries() {
    let doc = r#"{"a":{"b":1,"c.d":2,"abc":3},"list":[{"b":1},{"c":2},{"b":3,"s":"xyz"}]}"#;
    assert_eq!(get(doc, "a.b"), Some(json("1")));
    assert_eq!(get(doc, r"a.c\.d"), Some(json("2")));
    assert_eq!(get(doc, "a.ab?"), Some(json("3")));
    assert_eq!(get(doc, "a.x*"), None);
    assert_eq!(get(doc, "list.1"), Some(json(r#"{"c":2}"#)));
    assert_eq!(get(doc, "list.#"), Some(json("3")));
    assert_eq!(get(doc, "list.#.b"), Some(json("[1,3]")));
    assert_eq!(get(doc, "list.#(b>1).b"), Some(json("3")));
    assert_eq!(get(doc, "list.#(b>=1)#.b"), Some(json("[1,3]")));
    assert_eq!(get(doc, r#"list.#(s%"x*").b"#), Some(json("3")));
    assert_eq!(
        get(doc, r#"list.#(s!%"y*")#"#),
        Some(json(r#"[{"b":3,"s":"xyz"}]"#))
    );
    assert_eq!(get(doc, "list.#(b==9)"), None);
    assert_eq!(get(doc, "list.#(b==9)#"), Some(json("[]")));
}

#[test]
fn gjson_finds_nothing_for_unsupported_syntax() {
    let doc = r#"{"a":[1,2],"b":{"c":1}}"#;
    for path in [
        "a|@reverse",
        "@this",
        "!true",
        "[a,b]",
        "{a}",
        "..a",
        "b.c|x",
    ] {
        assert_eq!(get(doc, path), None, "{path}");
    }
}

#[test]
fn match_limit_wildcards() {
    assert!(match_limit("hello", "h*o"));
    assert!(match_limit("hello", "h?llo"));
    assert!(match_limit("h*llo", r"h\*llo"));
    assert!(!match_limit("hallo", r"h\*llo"));
    assert!(!match_limit("hello", "h?lo"));
    assert!(match_limit("", "*"));
}

fn set(doc: &str, path: &str, value: &str) -> Result<Value, SetError> {
    let mut doc = json(doc);
    sjson::set(&mut doc, path, &json(value)).map(|()| doc)
}

fn delete(doc: &str, path: &str) -> Result<Value, SetError> {
    let mut doc = json(doc);
    sjson::delete(&mut doc, path).map(|()| doc)
}

#[test]
fn sjson_sets_and_builds_paths() {
    assert_eq!(set("{}", "a.b.c", "1"), Ok(json(r#"{"a":{"b":{"c":1}}}"#)));
    assert_eq!(set("{}", "a.0", "1"), Ok(json(r#"{"a":[1]}"#)));
    assert_eq!(set("{}", "a.:0", "1"), Ok(json(r#"{"a":{"0":1}}"#)));
    assert_eq!(
        set(r#"{"a":[1]}"#, "a.2", "3"),
        Ok(json(r#"{"a":[1,null,3]}"#))
    );
    assert_eq!(set(r#"{"a":[1]}"#, "a.-1", "2"), Ok(json(r#"{"a":[1,2]}"#)));
    assert_eq!(
        set(r#"{"a":"x"}"#, "a.b", "1"),
        Ok(json(r#"{"a":{"b":1}}"#))
    );
    assert_eq!(set(r#"{"a.b":1}"#, r"a\.b", "2"), Ok(json(r#"{"a.b":2}"#)));
    assert_eq!(
        set(r#"{"a":[{"v":1},{"v":2}]}"#, "a.#.v", "0"),
        Ok(json(r#"{"a":[{"v":0},{"v":0}]}"#))
    );
    assert_eq!(set(r#"{"a":[]}"#, "a.x", "1"), Err(SetError::NonNumericKey));
    assert_eq!(set(r#"{"a":[]}"#, "a.5000", "1"), Err(SetError::TooFar));
    assert_eq!(set("{}", "", "1"), Err(SetError::EmptyPath));
}

#[test]
fn sjson_deletes_paths() {
    assert_eq!(delete(r#"{"a":[1,2]}"#, "a.0"), Ok(json(r#"{"a":[2]}"#)));
    assert_eq!(delete(r#"{"a":[1,2]}"#, "a.-1"), Ok(json(r#"{"a":[1]}"#)));
    assert_eq!(
        delete(r#"{"a":{"b":1,"c":2}}"#, "a.b"),
        Ok(json(r#"{"a":{"c":2}}"#))
    );
    assert_eq!(delete(r#"{"a":{}}"#, "a.x.y"), Ok(json(r#"{"a":{}}"#)));
    assert_eq!(
        delete(r#"{"a":[{"b":1}]}"#, "a.#.b"),
        Err(SetError::ComplexDelete)
    );
}

#[test]
fn paths_resolve_queries_to_indexes() {
    let doc = json(r#"{"items":[{"t":"x"},{"t":"y"},{"t":"x"}]}"#);
    assert_eq!(
        path::resolve(&doc, r#"items.#(t=="x")#.v"#),
        ["items.0.v", "items.2.v"]
    );
    assert_eq!(path::resolve(&doc, r#"items.#(t=="x").v"#), ["items.0.v"]);
    assert_eq!(
        path::resolve(&doc, r#"items.#(t=="z").v"#),
        Vec::<String>::new()
    );
    assert_eq!(path::resolve(&doc, " plain.path "), ["plain.path"]);
    assert_eq!(path::build_path(" request ", ".tools"), "request.tools");
    assert_eq!(path::build_path("", " tools "), "tools");
    assert!(path::targets_path("a.b", "a"));
    assert!(path::targets_path("a", "a.b"));
    assert!(!path::targets_path("ab", "a"));
}

#[test]
fn models_match_with_wildcards_and_suffixes() {
    assert!(matchers::match_model_pattern(b"gpt-*", b"gpt-5"));
    assert!(matchers::match_model_pattern(b" *-codex ", b"gpt-5-codex"));
    assert!(!matchers::match_model_pattern(b"gpt-*", b"claude"));
    assert!(!matchers::match_model_pattern(b"", b""));
    assert_eq!(
        matchers::candidates("gpt-5", "GPT-5(high)"),
        ["gpt-5", "GPT-5(high)"]
    );
}

/// A rule matches the model the client named with or without its thinking
/// suffix, defaults write a path once, the first rule winning, and
/// overrides write it each time, the last winning.
#[test]
fn defaults_first_overrides_last() {
    let config = r#"
payload:
  default:
    - models:
        - name: gpt-5(high)
      params:
        a: first
    - models:
        - name: gpt-5
      params:
        a: second
        b: second
  override:
    - models:
        - name: gpt-*
      params:
        c: first
    - models:
        - name: gpt-5
      params:
        c: second
"#;
    let args = Args {
        model: "upstream-model",
        requested: "gpt-5(high)",
        ..Args::default()
    };
    let out = args.run(config, "{}");
    assert_eq!(out, json(r#"{"a":"first","b":"second","c":"second"}"#));
}

/// A rule's values are written as Go writes them, and a value that can't be
/// written is dropped at load with the rest of its rule kept.
#[test]
fn values_are_encoded_at_load() {
    let config = r#"
payload:
  override:
    - models:
        - name: m
      params:
        float: 1.0
        big: 1e21
        list: [1, a, true]
        map: {z: 1, a: 2}
        nan: .nan
  override-raw:
    - models:
        - name: m
      params:
        raw: '{"k":[1,2]}'
        nothing: ~
"#;
    let out = Args {
        model: "m",
        ..Args::default()
    }
    .run(config, "{}");
    assert_eq!(
        serde_json::to_string(&out).ok().as_deref(),
        Some(
            r#"{"float":1,"big":1000000000000000000000,"list":[1,"a",true],"map":{"a":2,"z":1},"raw":{"k":[1,2]}}"#
        )
    );
}

/// The rules installed for every call win over the executor's config, and
/// a call without a config reads none.
#[test]
fn calls_read_the_installed_rules() {
    let own = Config::parse("payload:\n  override:\n    - models:\n        - name: m\n      params:\n        own: true\n")
        .expect("config parses");
    let installed = Arc::new(rules(
        "payload:\n  override:\n    - models:\n        - name: m\n      params:\n        installed: true\n",
    ));

    let selected = select(Some(&own), || Some(Arc::clone(&installed)));
    assert!(matches!(selected, Selected::Installed(_)));
    let out = Args {
        model: "m",
        ..Args::default()
    }
    .apply(selected.get(), "{}")
    .0;
    assert_eq!(out, json(r#"{"installed":true}"#));

    let selected = select(Some(&own), || None);
    let out = Args {
        model: "m",
        ..Args::default()
    }
    .apply(selected.get(), "{}")
    .0;
    assert_eq!(out, json(r#"{"own":true}"#));

    assert!(select(None, || Some(installed)).get().is_none());
    assert!(Rules::default().filter.is_empty());
}

/// Rules under a root, tracked paths and a filter of the matches of a
/// query, removed from the last.
#[test]
fn root_tracking_and_filters() {
    let config = r#"
payload:
  filter:
    - models:
        - name: m
          protocol: gemini
      params:
        - 'contents.#(role=="model")#'
  override:
    - models:
        - name: m
          protocol: gemini
      params:
        generationConfig.temperature: 0.5
"#;
    let args = Args {
        model: "m",
        protocol: "gemini",
        root: "request",
        tracked: &[
            "request.generationConfig",
            " request.contents ",
            "request.tools",
        ],
        ..Args::default()
    };
    let (out, touched) = args.apply(
        Some(&rules(config)),
        r#"{"request":{"contents":[{"role":"model"},{"role":"user"},{"role":"model"}]}}"#,
    );
    assert_eq!(
        out,
        json(
            r#"{"request":{"contents":[{"role":"user"}],"generationConfig":{"temperature":0.5}}}"#
        )
    );
    assert_eq!(
        touched.iter().collect::<Vec<_>>(),
        ["request.contents", "request.generationConfig"]
    );

    let args = Args {
        protocol: "claude",
        ..args
    };
    let (out, touched) = args.apply(Some(&rules(config)), r#"{"request":{}}"#);
    assert_eq!(out, json(r#"{"request":{}}"#));
    assert!(touched.is_empty());
}
