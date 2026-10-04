//! Hand-written cases for the config's payload rules: the scenarios of
//! upstream's tests, then the edges of each part: model, protocol, header
//! and body conditions; defaults checked against the client's request;
//! overrides and filters through `#(query)` keys, projections and the
//! call's root; the values each kind of YAML scalar writes; the tracked
//! paths; `disable-image-generation`; and a Codex client's integer pass.

use serde_json::{Value, json};

use super::Case;

const CODEX_UA: &str = "codex_cli_rs/0.1";

/// A case sending `body` for `model` with `options`.
fn case(name: &str, model: &str, body: &str, options: Value) -> Case {
    Case::new(name, model, body).with_options(options)
}

/// Options for a call with `config`, otherwise empty.
fn options(config: &str) -> Value {
    json!({
        "config": config,
        "no_config": false,
        "executor": "",
        "protocol": "",
        "from": "",
        "root": "",
        "original": null,
        "requested_model": "",
        "request_path": "",
        "headers": [],
        "tracked": [],
    })
}

/// `options` with `key` set to `value`.
fn with(mut options: Value, key: &str, value: impl Into<Value>) -> Value {
    options[key] = value.into();
    options
}

/// A config with one `section` rule for models named `name` (and protocol
/// `protocol` unless empty), with `params` as YAML lines under `params:`.
fn rule(section: &str, name: &str, protocol: &str, params: &str) -> String {
    let protocol = if protocol.is_empty() {
        String::new()
    } else {
        format!("\n          protocol: {protocol}")
    };
    let params = params
        .lines()
        .map(|line| format!("        {line}\n"))
        .collect::<String>();
    format!(
        "payload:\n  {section}:\n    - models:\n        - name: '{name}'{protocol}\n      params:\n{params}"
    )
}

/// The hand-written cases for `payload/apply`.
pub fn applies() -> Vec<Case> {
    let mut cases = upstream_cases();
    cases.extend(matching_cases());
    cases.extend(default_cases());
    cases.extend(path_cases());
    cases.extend(value_cases());
    cases.extend(image_cases());
    cases.extend(integer_cases());
    cases
}

/// The scenarios of upstream's tests.
fn upstream_cases() -> Vec<Case> {
    let image_body = r#"{"tools":[{"type":"image_generation","output_format":"png"},{"type":"function","name":"f1"}],"tool_choice":{"type":"image_generation"}}"#;
    let additional_tools = r#"
payload:
  filter:
    - models:
        - name: gpt-*
          protocol: codex
      params:
        - 'input.#(type=="additional_tools")#.tools.#(name=="functions")#.tools.#(name=="apply_patch")#'
"#;
    let header_gate = r#"
payload:
  override:
    - models:
        - name: gpt-*
          protocol: openai
          headers:
            X-Client-Tier: tenant-*-region-*
      params:
        metadata.enabled: true
"#;
    let from_gate = r#"
payload:
  override:
    - models:
        - name: gpt-*
          protocol: openai
          from-protocol: responses
      params:
        metadata.source: responses
    - models:
        - name: gpt-*
          protocol: openai
          from-protocol: openai
      params:
        metadata.source: openai
"#;
    let conditions = r#"
payload:
  override:
    - models:
        - name: gpt-*
          match:
            - metadata.client: codex
            - 'tools.#(type=="web_search").enabled': true
          not-match:
            - metadata.mode: dev
          exist:
            - 'tools.#(type=="web_search").type'
          not-exist:
            - metadata.missing
            - metadata.null_value
      params:
        metadata.applied: true
"#;
    let condition_body = r#"{"model":"gpt-5.4","metadata":{"client":"codex","mode":"prod","null_value":null},"tools":[{"type":"function"},{"type":"web_search","enabled":true}]}"#;
    let claude = |section: &str, params: &str| rule(section, "claude-opus-5", "claude", params);
    let automatic = r#"{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#;
    let tracked = |name: &str, body: &str, original: Option<&str>, config: String| {
        let options = with(options(&config), "protocol", "claude");
        let options = with(options, "from", "claude");
        let options = with(options, "requested_model", "claude-opus-5");
        let options = with(options, "tracked", json!(["context_management"]));
        case(
            name,
            "claude-opus-5",
            body,
            with(options, "original", original.map(Value::from)),
        )
    };
    let with_automatic = format!(r#"{{"model":"claude-opus-5","context_management":{automatic}}}"#);
    vec![
        case(
            "upstream-image-removes-tools-entry",
            "gpt-5.4",
            r#"{"tools":[{"type":"image_generation","output_format":"png"},{"type":"function","name":"f1"}]}"#,
            with(
                options("disable-image-generation: true\n"),
                "protocol",
                "openai-response",
            ),
        ),
        case(
            "upstream-image-removes-tools-entry-with-root",
            "gpt-5.4",
            r#"{"request":{"tools":[{"type":"image_generation"},{"type":"web_search"}]}}"#,
            with(
                with(
                    options("disable-image-generation: true\n"),
                    "protocol",
                    "antigravity",
                ),
                "root",
                "request",
            ),
        ),
        case(
            "upstream-image-removes-tool-choice-by-type",
            "gpt-5.4",
            image_body,
            with(
                options("disable-image-generation: true\n"),
                "protocol",
                "openai-response",
            ),
        ),
        case(
            "upstream-image-removes-tool-choice-by-name-with-root",
            "gpt-5.4",
            r#"{"request":{"tools":[{"type":"image_generation"},{"type":"web_search"}],"tool_choice":{"type":"tool","name":"image_generation"}}}"#,
            with(
                with(
                    options("disable-image-generation: true\n"),
                    "protocol",
                    "antigravity",
                ),
                "root",
                "request",
            ),
        ),
        case(
            "upstream-image-chat-keeps-images-endpoint",
            "gpt-5.4",
            image_body,
            with(
                with(
                    options("disable-image-generation: chat\n"),
                    "protocol",
                    "openai-response",
                ),
                "request_path",
                "/v1/images/generations",
            ),
        ),
        case(
            "upstream-image-passthrough-keeps-payload",
            "gpt-5.4",
            image_body,
            with(
                with(
                    options("disable-image-generation: passthrough\n"),
                    "protocol",
                    "openai-response",
                ),
                "request_path",
                "/v1/responses",
            ),
        ),
        case(
            "upstream-image-override-restores",
            "gpt-5.4",
            image_body,
            with(
                options(&format!(
                    "disable-image-generation: true\n{}",
                    rule(
                        "override-raw",
                        "gpt-5.4",
                        "openai-response",
                        "tools: '[{\"type\":\"image_generation\"},{\"type\":\"function\",\"name\":\"f1\"}]'\ntool_choice: '{\"type\":\"image_generation\"}'"
                    )
                )),
                "protocol",
                "openai-response",
            ),
        ),
        case(
            "upstream-header-gate-matches",
            "gpt-5.4",
            r#"{"model":"gpt-5.4"}"#,
            with(
                with(
                    with(options(header_gate), "protocol", "openai"),
                    "from",
                    "responses",
                ),
                "headers",
                json!([["X-Client-Tier", "tenant-alpha-region-us"]]),
            ),
        ),
        case(
            "upstream-header-gate-mismatch",
            "gpt-5.4",
            r#"{"model":"gpt-5.4"}"#,
            with(
                with(
                    with(options(header_gate), "protocol", "openai"),
                    "from",
                    "responses",
                ),
                "headers",
                json!([["X-Client-Tier", "tenant-alpha"]]),
            ),
        ),
        case(
            "upstream-from-protocol-responses",
            "gpt-5.4",
            r#"{"model":"gpt-5.4"}"#,
            with(
                with(options(from_gate), "protocol", "openai"),
                "from",
                "openai-response",
            ),
        ),
        case(
            "upstream-from-protocol-openai",
            "gpt-5.4",
            r#"{"model":"gpt-5.4"}"#,
            with(
                with(options(from_gate), "protocol", "openai"),
                "from",
                "openai",
            ),
        ),
        case(
            "upstream-conditions-narrow-rule",
            "gpt-5.4",
            condition_body,
            with(
                with(options(conditions), "protocol", "openai"),
                "from",
                "responses",
            ),
        ),
        case(
            "upstream-conditions-skip-rule",
            "gpt-5.4",
            r#"{"model":"gpt-5.4","metadata":{"client":"other","mode":"dev","null_value":null}}"#,
            with(
                with(options(conditions), "protocol", "openai"),
                "from",
                "responses",
            ),
        ),
        tracked(
            "upstream-tracked-default",
            r#"{"model":"claude-opus-5"}"#,
            Some(r#"{"model":"claude-opus-5"}"#),
            claude(
                "default",
                "context_management:\n  edits:\n    - type: default",
            ),
        ),
        tracked(
            "upstream-tracked-raw-default",
            r#"{"model":"claude-opus-5"}"#,
            Some(r#"{"model":"claude-opus-5"}"#),
            claude(
                "default-raw",
                "context_management: '{\"edits\":[{\"type\":\"raw_default\"}]}'",
            ),
        ),
        tracked(
            "upstream-tracked-canonical-descendant-override",
            &with_automatic,
            None,
            claude("override", "context_management.edits.0.keep: all"),
        ),
        tracked(
            "upstream-tracked-identical-raw-override",
            &with_automatic,
            None,
            claude(
                "override-raw",
                &format!("context_management: '{automatic}'"),
            ),
        ),
        tracked(
            "upstream-tracked-filter-already-absent",
            r#"{"model":"claude-opus-5"}"#,
            None,
            claude("filter", "- context_management"),
        ),
        tracked(
            "upstream-tracked-unrelated-override",
            r#"{"model":"claude-opus-5"}"#,
            None,
            claude("override", "thinking.type: enabled"),
        ),
        tracked(
            "upstream-tracked-nonmatching-override",
            r#"{"model":"claude-opus-5"}"#,
            None,
            rule(
                "override",
                "other-model",
                "claude",
                "context_management:\n  edits: []",
            ),
        ),
        tracked(
            "upstream-tracked-default-skipped-for-caller",
            r#"{"model":"claude-opus-5","context_management":{"edits":[{"type":"caller"}]}}"#,
            Some(r#"{"model":"claude-opus-5","context_management":{"edits":[{"type":"caller"}]}}"#),
            claude(
                "default",
                "context_management:\n  edits:\n    - type: default",
            ),
        ),
        case(
            "upstream-canonical-overrides",
            "gpt-test",
            r#"{"model":"gpt-test","stream":true,"metadata":{"source":"executor"},"messages":[]}"#,
            with(
                options(&format!(
                    "{}  override-raw:\n    - models:\n        - name: gpt-test\n          protocol: openai\n      params:\n        metadata: '{{\"source\":\"executor\"}}'\n",
                    rule(
                        "override",
                        "gpt-test",
                        "openai",
                        "stream: true\nmodel: gpt-test"
                    )
                )),
                "protocol",
                "openai",
            ),
        ),
        case(
            "upstream-projection-override",
            "gpt-test",
            r#"{"items":[{"value":1},{"value":2}]}"#,
            with(
                options(&rule(
                    "override",
                    "gpt-test",
                    "openai",
                    "items.#.value: [1, 2]",
                )),
                "protocol",
                "openai",
            ),
        ),
        case(
            "upstream-projection-override-raw",
            "gpt-test",
            r#"{"items":[{"value":1},{"value":2}]}"#,
            with(
                options(&rule(
                    "override-raw",
                    "gpt-test",
                    "openai",
                    "items.#.value: '[1,2]'",
                )),
                "protocol",
                "openai",
            ),
        ),
        case(
            "upstream-additional-tools-across-namespaces",
            "gpt-5-codex",
            r#"{
                "model": "gpt-5-codex",
                "input": [{"type": "additional_tools", "role": "developer", "tools": [
                    {"type": "namespace", "name": "functions", "tools": [
                        {"type": "custom", "name": "apply_patch", "description": "patch"},
                        {"type": "custom", "name": "exec_command", "description": "exec"}]},
                    {"type": "namespace", "name": "collaboration", "tools": [
                        {"type": "custom", "name": "share", "description": "share"}]}]}]
            }"#,
            with(options(additional_tools), "protocol", "codex"),
        ),
        case(
            "upstream-additional-tools-no-match",
            "gpt-5-codex",
            r#"{"model":"gpt-5-codex","input":[{"type":"additional_tools","role":"developer","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec_command","description":"exec"}]}]}]}"#,
            with(options(additional_tools), "protocol", "codex"),
        ),
        case(
            "upstream-additional-tools-multiple-elements",
            "gpt-5-codex",
            r#"{"model":"gpt-5-codex","input":[
                {"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch"}]}]},
                {"type":"message","role":"user","content":"hello"},
                {"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch"},{"type":"custom","name":"view_image"}]}]}]}"#,
            with(options(additional_tools), "protocol", "codex"),
        ),
        case(
            "upstream-additional-tools-same-array",
            "gpt-5-codex",
            r#"{"model":"gpt-5-codex","input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[
                {"type":"custom","name":"apply_patch","id":"p1"},
                {"type":"custom","name":"exec_command"},
                {"type":"custom","name":"apply_patch","id":"p2"}]}]}]}"#,
            with(options(additional_tools), "protocol", "codex"),
        ),
    ]
}

/// Which rules apply: model names and wildcards, the thinking suffix,
/// protocols, headers and conditions.
fn matching_cases() -> Vec<Case> {
    let body =
        r#"{"model":"x","metadata":{"n":1,"f":1.0,"s":"a","b":true,"z":null,"o":{"k":[1,"2"]}}}"#;
    let tagged = |name: &str| rule("override", name, "", "tag: hit");
    let conditions = |condition: &str| {
        format!(
            "payload:\n  override:\n    - models:\n        - name: '*'\n          {condition}\n      params:\n        tag: hit\n"
        )
    };
    let mut cases = Vec::new();
    for (name, pattern, model, requested) in [
        ("exact", "gpt-5", "gpt-5", ""),
        ("case", "GPT-5", "gpt-5", ""),
        ("prefix-star", "gpt-*", "gpt-5", ""),
        ("suffix-star", "*-codex", "gpt-5-codex", ""),
        ("middle-star", "gpt-*-codex", "gpt-5.1-codex", ""),
        ("double-star", "g**5", "gpt-5", ""),
        ("star-only", "*", "anything", ""),
        ("no-match", "claude-*", "gpt-5", ""),
        ("spaces", "  gpt-5  ", " gpt-5 ", ""),
        ("requested", "alias-model", "upstream-model", "alias-model"),
        (
            "requested-suffix",
            "alias-model(high)",
            "upstream-model",
            "alias-model(high)",
        ),
        (
            "requested-base",
            "alias-model",
            "upstream-model",
            " alias-model(high) ",
        ),
        ("only-requested", "alias", "", "alias"),
        ("neither", "*", "", "  "),
        ("question-mark-literal", "gpt-?", "gpt-5", ""),
        ("unicode", "modèle-*", "modèle-ü", ""),
    ] {
        cases.push(case(
            &format!("match-model-{name}"),
            model,
            body,
            with(options(&tagged(pattern)), "requested_model", requested),
        ));
    }
    for (name, protocol, from) in [
        ("protocol-match", "openai", ""),
        ("protocol-case", "OpenAI", ""),
        ("protocol-mismatch", "claude", ""),
        ("protocol-empty", "", ""),
        ("from-chat", "openai", "openai"),
        ("from-responses", "openai", "openai-response"),
        ("from-claude", "openai", "claude"),
    ] {
        let config = "payload:\n  override:\n    - models:\n        - name: m\n          protocol: openai\n      params:\n        a: 1\n    - models:\n        - name: m\n          from-protocol: chat\n      params:\n        b: 1\n    - models:\n        - name: m\n          from-protocol: Responses\n      params:\n        c: 1\n    - models:\n        - name: m\n          from-protocol: anthropic\n      params:\n        d: 1\n";
        cases.push(case(
            &format!("match-{name}"),
            "m",
            "{}",
            with(with(options(config), "protocol", protocol), "from", from),
        ));
    }
    let headers_config = "payload:\n  override:\n    - models:\n        - name: m\n          headers:\n            X-One: 'a*'\n            ' x-two ': '*'\n      params:\n        tag: hit\n";
    for (name, headers) in [
        ("both", json!([["X-One", "abc"], ["X-Two", ""]])),
        (
            "second-value",
            json!([["X-One", "zzz"], ["x-one", "a1"], ["X-Two", "v"]]),
        ),
        ("missing", json!([["X-One", "abc"]])),
        ("mismatch", json!([["X-One", "b"], ["X-Two", "v"]])),
    ] {
        cases.push(case(
            &format!("match-headers-{name}"),
            "m",
            "{}",
            with(options(headers_config), "headers", headers),
        ));
    }
    for (name, condition) in [
        ("int-equals-float", "match:\n            - metadata.f: 1"),
        ("float-equals-int", "match:\n            - metadata.n: 1.0"),
        ("string", "match:\n            - metadata.s: a"),
        ("string-vs-number", "match:\n            - metadata.n: '1'"),
        ("bool", "match:\n            - metadata.b: true"),
        ("null", "match:\n            - metadata.z: null"),
        (
            "missing-null",
            "match:\n            - metadata.missing: null",
        ),
        ("object", "match:\n            - metadata.o: {k: [1, '2']}"),
        (
            "object-mismatch",
            "match:\n            - metadata.o: {k: [1, 2]}",
        ),
        (
            "two-keys",
            "match:\n            - {metadata.n: 1, metadata.s: a}",
        ),
        (
            "two-keys-one-off",
            "match:\n            - {metadata.n: 1, metadata.s: b}",
        ),
        ("not-match-other", "not-match:\n            - metadata.s: b"),
        ("not-match-same", "not-match:\n            - metadata.s: a"),
        ("exist", "exist:\n            - metadata.o.k.1"),
        ("exist-null", "exist:\n            - metadata.z"),
        ("not-exist-null", "not-exist:\n            - metadata.z"),
        ("not-exist-present", "not-exist:\n            - metadata.s"),
        ("count", "match:\n            - metadata.o.k.#: 2"),
        ("query", "exist:\n            - 'metadata.o.k.#(==\"2\")'"),
        ("wildcard-key", "match:\n            - metadata.o*.k.0: 1"),
    ] {
        cases.push(case(
            &format!("match-condition-{name}"),
            "m",
            body,
            options(&conditions(condition)),
        ));
    }
    cases
}

/// Defaults: the client's request decides, the first write wins, and a
/// default never touches what a client sent.
fn default_cases() -> Vec<Case> {
    // One param a rule: upstream writes a rule's params in Go's random map
    // order, which would order the keys they add.
    let one = |section: &str, params: &[&str]| {
        let rules: String = params
            .iter()
            .map(|param| {
                format!("    - models:\n        - name: m\n      params:\n        {param}\n")
            })
            .collect();
        format!("  {section}:\n{rules}")
    };
    let config = format!(
        "payload:\n{}{}",
        one(
            "default",
            &["a: first", "nested.x: 1", "a: second", "b: second"]
        ),
        one("default-raw", &["b: '\"raw\"'", "c: '{\"r\":[1,2.50]}'"]),
    );
    let config = config.as_str();
    vec![
        case("default-empty-body", "m", "{}", options(config)),
        case(
            "default-body-has",
            "m",
            r#"{"a":"client","nested":{"x":0}}"#,
            options(config),
        ),
        case(
            "default-original-has",
            "m",
            "{}",
            with(options(config), "original", r#"{"a":"client","c":null}"#),
        ),
        case(
            "default-original-lacks",
            "m",
            r#"{"a":"translated"}"#,
            with(options(config), "original", "{}"),
        ),
        case(
            "default-original-empty",
            "m",
            r#"{"a":"translated"}"#,
            with(options(config), "original", ""),
        ),
        case(
            "default-under-root",
            "m",
            r#"{"request":{"a":1}}"#,
            with(options(config), "root", "request"),
        ),
        case(
            "default-query-paths",
            "m",
            r#"{"items":[{"t":"x"},{"t":"y","v":0},{"t":"x","v":2}]}"#,
            options(&rule("default", "m", "", "'items.#(t==\"x\")#.v': 9")),
        ),
        case(
            "default-query-paths-original",
            "m",
            r#"{"items":[{"t":"x"},{"t":"y"}]}"#,
            with(
                options(&rule("default", "m", "", "'items.#(t==\"x\")#.v': 9")),
                "original",
                r#"{"items":[{"v":1}]}"#,
            ),
        ),
        case(
            "default-tracked",
            "m",
            r#"{"b":0}"#,
            with(
                options(config),
                "tracked",
                json!(["a", "b", "nested", "c.r", " ", "z"]),
            ),
        ),
    ]
}

/// Paths: escapes, wildcards, indexes, appends, queries and projections
/// in overrides and filters.
fn path_cases() -> Vec<Case> {
    let body = r#"{"a":{"b.c":1,"list":[1,2,3],"objs":[{"k":"x","v":1},{"k":"y","v":2},{"k":"x","v":3}]},"s":"text"}"#;
    let mut cases = Vec::new();
    for (name, section, params) in [
        ("escaped-key", "override", r"'a.b\.c': 2"),
        ("new-nested", "override", "x.y.z: 1"),
        ("index", "override", "a.list.1: 9"),
        ("append", "override", "a.list.-1: 4"),
        ("pad", "override", "a.list.5: 6"),
        ("forced-key", "override", "a.:0: k"),
        ("numeric-key-new", "override", "n.0: k"),
        ("into-string", "override", "s.t: 1"),
        ("into-array-key", "override", "a.list.k: 1"),
        ("wildcard", "override", "a.ob*.0.v: 0"),
        ("projection", "override", "a.objs.#.v: 0"),
        ("query-first", "override", "'a.objs.#(k==\"x\").v': 0"),
        ("query-all", "override", "'a.objs.#(k==\"x\")#.v': 0"),
        ("query-none", "override", "'a.objs.#(k==\"z\")#.v': 0"),
        ("query-and", "override", "'a.objs.#(k==\"x\"&&v>1)#.v': 0"),
        ("query-or", "override", "'a.objs.#(k==\"y\"||v==1)#.v': 0"),
        ("query-like", "override", "'a.objs.#(k%\"*\")#.w': 0"),
        ("count", "override", "a.list.#: 0"),
        ("root-replace", "override", "a: 0"),
        ("leading-dot", "override", ".s: replaced"),
        ("filter-key", "filter", "- a.b\\.c"),
        ("filter-index", "filter", "- a.list.0"),
        ("filter-last", "filter", "- a.list.-1"),
        ("filter-missing", "filter", "- a.nothing.here"),
        ("filter-query-all", "filter", "- 'a.objs.#(k==\"x\")#'"),
        ("filter-query-field", "filter", "- 'a.objs.#(k==\"x\")#.v'"),
        ("filter-projection", "filter", "- a.objs.#.v"),
        ("filter-wildcard", "filter", "- a.l*"),
        ("filter-two", "filter", "- s\n- a.list"),
    ] {
        cases.push(case(
            &format!("path-{name}"),
            "m",
            body,
            with(
                options(&rule(section, "m", "", params)),
                "tracked",
                json!(["a", "a.objs", "s", "x"]),
            ),
        ));
    }
    cases.push(case(
        "path-root-override-and-filter",
        "m",
        r#"{"request":{"contents":[{"role":"model"},{"role":"user"},{"role":"model"}]}}"#,
        with(
            with(
                options("payload:\n  override:\n    - models:\n        - name: m\n      params:\n        generationConfig.temperature: 0.5\n  filter:\n    - models:\n        - name: m\n      params:\n        - 'contents.#(role==\"model\")#'\n"),
                "root",
                " request ",
            ),
            "tracked",
            json!(["request.contents", "request.generationConfig.temperature", "request"]),
        ),
    ));
    cases.push(case(
        "path-override-order",
        "m",
        "{}",
        options("payload:\n  override:\n    - models:\n        - name: m\n      params:\n        v: first\n    - models:\n        - name: '*'\n      params:\n        v: second\n  override-raw:\n    - models:\n        - name: m\n      params:\n        w: '[1]'\n"),
    ));
    cases.push(case(
        "path-conditions-see-earlier-rules",
        "m",
        "{}",
        options("payload:\n  override:\n    - models:\n        - name: m\n      params:\n        flag: true\n    - models:\n        - name: m\n          exist:\n            - flag\n      params:\n        seen: true\n"),
    ));
    cases
}

/// The values each kind of YAML scalar and raw JSON writes.
fn value_cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for (name, value) in [
        ("int", "42"),
        ("negative", "-7"),
        ("big-int", "9223372036854775807"),
        ("uint", "18446744073709551615"),
        ("float", "1.5"),
        ("float-whole", "1.0"),
        ("float-tiny", "0.1"),
        ("float-exp", "1e21"),
        ("float-small-exp", "1.5e-7"),
        ("negative-zero", "-0.0"),
        ("hex", "0x1F"),
        ("octal", "0o17"),
        ("bool", "true"),
        ("yes-string", "yes"),
        ("null", "null"),
        ("tilde", "~"),
        ("string", "hello"),
        ("quoted-number", "'12'"),
        ("unicode", "'é ü 🚀 <&>'"),
        ("escapes", "\"tab\\tquote\\\"\""),
        ("list", "[1, two, 3.0, null, {k: v}]"),
        ("map", "{z: 1, a: [true], m: {y: 2, b: 1}}"),
        ("empty-list", "[]"),
        ("empty-map", "{}"),
        ("timestamp", "2001-12-14"),
        ("binary", "!!binary aGVsbG8="),
    ] {
        cases.push(case(
            &format!("value-{name}"),
            "m",
            r#"{"keep":1}"#,
            options(&rule("override", "m", "", &format!("v: {value}"))),
        ));
    }
    for (name, value) in [
        ("object", r#"'{"b":1,"a":[1.50,"x"]}'"#),
        ("number-text", "'1.50e+3'"),
        ("string", r#"'"text"'"#),
        ("spaces", r#"'  {"a" : 1}  '"#),
        ("number-value", "12"),
        ("bool-value", "true"),
        ("list-value", "[1, 2]"),
        ("map-value", "{a: 1}"),
        ("escaped", r#"'"\u00e9\n"'"#),
    ] {
        cases.push(case(
            &format!("value-raw-{name}"),
            "m",
            r#"{"keep":1}"#,
            options(&rule("override-raw", "m", "", &format!("v: {value}"))),
        ));
    }
    cases.push(case(
        "value-raw-invalid-drops-rule",
        "m",
        r#"{"keep":1}"#,
        options(&format!(
            "{}  override:\n    - models:\n        - name: m\n      params:\n        x: 1\n",
            rule("override-raw", "m", "", "v: '{nope'\nw: '2'")
        )),
    ));
    cases.push(
        case(
            "value-nan",
            "m",
            r#"{"keep":1}"#,
            options(&rule("override", "m", "", "v: .nan\nw: .inf")),
        )
        .known_difference(
            "a NaN or infinite value is dropped at load; upstream writes invalid JSON",
        ),
    );
    cases.push(case(
        "config-error",
        "m",
        "{}",
        options("payload: [unclosed"),
    ));
    cases
}

/// `disable-image-generation` on each endpoint, under a root, and with
/// defaults that check the request before the strip.
fn image_cases() -> Vec<Case> {
    let body = r#"{"tools":[{"type":"image_generation"},{"type":"IMAGE_GENERATION"},{"type":"function","name":"image_generation"},"image_generation"],"tool_choice":" Image_Generation "}"#;
    let mut cases = Vec::new();
    for mode in ["true", "false", "chat", "passthrough"] {
        for path in [
            "",
            "/v1/responses",
            "/v1/images/generations",
            "/backend/images/edits ",
            "/v1/images",
        ] {
            cases.push(case(
                &format!("image-{mode}-{}", path.trim().replace('/', "_")),
                "m",
                body,
                with(
                    options(&format!("disable-image-generation: {mode}\n")),
                    "request_path",
                    path,
                ),
            ));
        }
    }
    for (name, choice) in [
        ("choice-type", r#"{"type":"image_generation"}"#),
        (
            "choice-tool-name",
            r#"{"type":" TOOL ","name":" image_generation "}"#,
        ),
        (
            "choice-function",
            r#"{"type":"function","name":"image_generation"}"#,
        ),
        ("choice-number", "5"),
        ("choice-array", r#"["image_generation"]"#),
        ("choice-auto", r#""auto""#),
    ] {
        cases.push(case(
            &format!("image-{name}"),
            "m",
            &format!(r#"{{"tools":[{{"type":"function"}}],"tool_choice":{choice}}}"#),
            options("disable-image-generation: true\n"),
        ));
    }
    cases.push(case(
        "image-tools-not-array",
        "m",
        r#"{"tools":{"type":"image_generation"},"tool_choice":"image_generation"}"#,
        options("disable-image-generation: true\n"),
    ));
    cases.push(case(
        "image-defaults-see-client-request",
        "m",
        r#"{"tools":[{"type":"image_generation"}],"tool_choice":"image_generation"}"#,
        options(&format!(
            "disable-image-generation: true\n{}",
            rule(
                "default",
                "m",
                "",
                "tool_choice: auto\ntools: []\nparallel_tool_calls: false"
            )
        )),
    ));
    cases.push(case(
        "image-under-root",
        "m",
        r#"{"request":{"tools":[{"type":"image_generation"},{"type":"x"}],"tool_choice":{"type":"tool","name":"IMAGE_GENERATION"}}}"#,
        with(options("disable-image-generation: true\n"), "root", "request"),
    ));
    cases
}

/// A Codex client's tools declared `integer` again, for each executor,
/// with and without a config.
fn integer_cases() -> Vec<Case> {
    let body = r#"{"tools":[{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"yield_time_ms":{"type":"number"},"timeout_ms":{"type":["number","null"]},"ratio":{"type":"number","description":"fraction"}}}}]}"#;
    let mut cases = Vec::new();
    for (name, executor, user_agent) in [
        ("codex", "codex", CODEX_UA),
        ("codex-spaced", " CODEX ", CODEX_UA),
        ("codex-websockets", "codex-websockets", CODEX_UA),
        ("codex-underscore", "codex_websockets", CODEX_UA),
        ("xai", "xai", CODEX_UA),
        ("claude", "claude", CODEX_UA),
        ("empty", "", CODEX_UA),
        ("tui", "gemini", "codex-tui/0.154.0"),
        ("curl", "gemini", "curl/8.7.1"),
    ] {
        cases.push(case(
            &format!("integer-{name}"),
            "model",
            body,
            with(
                with(with(options(""), "no_config", true), "executor", executor),
                "headers",
                json!([["User-Agent", user_agent]]),
            ),
        ));
    }
    cases.push(case(
        "integer-with-rules",
        "model",
        body,
        with(
            with(
                options(&rule(
                    "override",
                    "model",
                    "",
                    "tools.0.parameters.properties.ratio.type: number",
                )),
                "executor",
                "claude",
            ),
            "headers",
            json!([["User-Agent", CODEX_UA]]),
        ),
    ));
    cases
}
