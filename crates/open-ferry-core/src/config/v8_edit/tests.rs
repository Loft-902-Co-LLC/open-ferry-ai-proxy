// Ported in part from CLIProxyAPI internal/config/config_v8_test.go
// (TestV8ExampleLoadsAndRoundTrips, TestV8ValidationRejectsLegacyWriteLayout),
// model_catalogs_test.go (TestModelCatalogConfigValidation),
// client_test.go (TestClientCodexEnableApplyPatch) and
// client_optimize_test.go (TestClientCodexOptimizeMultiAgentV2,
// TestClientCodexOptimizeMultiAgentV2Migration) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// The routes' tests, with upstream's expected files, are in
// open-ferry-management's `config_v8_write`.

use std::fs;

use super::*;
use crate::config::testing::TempDir;
use crate::config::v8::V8_CLIENT_PATHS;

/// A `method` edit of the keys of `path` split on `/` (the whole config
/// when empty), with the JSON `body`.
fn edit(method: V8Method, path: &str, body: &str) -> V8Edit {
    let path = if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').map(str::to_owned).collect()
    };
    V8Edit {
        method,
        path,
        body: body.as_bytes().to_vec(),
        yaml: false,
    }
}

/// A `PUT config.yaml` of `body`.
fn put_yaml(body: &str) -> V8Edit {
    V8Edit {
        yaml: true,
        ..edit(V8Method::Put, "", body)
    }
}

/// The file `edit` makes of the file `data`.
fn rendered(data: &str, edit: &V8Edit) -> Result<String, V8EditError> {
    render(data.as_bytes(), edit).map(|(out, _)| String::from_utf8(out).expect("utf-8"))
}

/// The value of `path` (dotted) in `data`.
fn value_of<'a>(root: &'a Node, path: &str) -> Option<&'a str> {
    yaml_path(root, path).map(|node| node.value.as_str())
}

/// The root of the YAML `data`.
fn root_of(data: &str) -> Node {
    let doc = unmarshal(data.as_bytes()).expect("yaml");
    doc.content.first().cloned().expect("root")
}

/// Checks that `data` is a valid v8 config.
fn assert_valid(data: &str) {
    if let Err(error) = validate_v8_config(data.as_bytes()) {
        panic!("{error}\n{data}");
    }
}

/// A `PATCH` merges mappings key by key, keeps the file's comments and the
/// keys it doesn't name, and moves the file into the v8 layout.
#[test]
fn patch_merges_mappings_key_by_key() {
    let data = "# head\nport: 8317 # port\nhost: 127.0.0.1\nrouting: {strategy: fill-first}\n";
    let out = rendered(
        data,
        &edit(V8Method::Patch, "", r#"{"server":{"port":8318}}"#),
    )
    .expect("patch");
    assert_valid(&out);
    let root = root_of(&out);
    assert_eq!(value_of(&root, "server.port"), Some("8318"));
    assert_eq!(value_of(&root, "server.host"), Some("127.0.0.1"));
    assert_eq!(value_of(&root, "routing.strategy"), Some("fill-first"));
    assert_eq!(value_of(&root, "config-version"), Some("8"));
    for comment in ["# head", "# port"] {
        assert!(out.contains(comment), "{comment}: {out}");
    }
}

/// A `PATCH` keeps a `null` rather than deleting the key, and a `PUT` of a
/// scalar keeps the old scalar's comments.
#[test]
fn patch_keeps_null_and_put_keeps_scalar_comments() {
    let data = "config-version: 8\nrouting:\n  # why\n  strategy: fill-first # chosen\n";
    let out = rendered(
        data,
        &edit(V8Method::Put, "routing/strategy", r#""round-robin""#),
    )
    .expect("put");
    assert!(out.contains("# why") && out.contains("# chosen"), "{out}");
    assert_eq!(
        value_of(&root_of(&out), "routing.strategy"),
        Some("round-robin")
    );
    let out = rendered(
        data,
        &edit(V8Method::Patch, "routing", r#"{"strategy":null}"#),
    )
    .expect("patch");
    let root = root_of(&out);
    let strategy = yaml_path(&root, "routing.strategy").expect("kept");
    assert_eq!(strategy.tag, NULL_TAG, "{out}");
}

/// A `DELETE` removes the key and the mappings it leaves empty, and keeps
/// other empty mappings and nulls.
#[test]
fn delete_removes_only_what_it_empties() {
    let data = "config-version: 8\nrouting: {retry: {request-retry: 3}}\nplugins: {configs: {sample: {options: {}, custom: null}}}\n";
    let out = rendered(
        data,
        &edit(V8Method::Delete, "routing/retry/request-retry", ""),
    )
    .expect("delete");
    let root = root_of(&out);
    assert!(yaml_path(&root, "routing").is_none(), "{out}");
    let options = yaml_path(&root, "plugins.configs.sample.options").expect("options");
    assert!(options.kind == Kind::Mapping && options.content.is_empty());
    let custom = yaml_path(&root, "plugins.configs.sample.custom").expect("custom");
    assert_eq!(custom.tag, NULL_TAG);

    for (path, error) in [
        ("", V8EditError::CannotDeleteConfig),
        ("routing/retry/max-retry-interval", V8EditError::NotFound),
        ("routing/retry/request-retry/deeper", V8EditError::NotFound),
        ("nope", V8EditError::NotFound),
    ] {
        let got = rendered(data, &edit(V8Method::Delete, path, ""));
        assert_eq!(got, Err(error), "{path}");
    }
}

/// The refusals of a body, a path and a result, each with the error the
/// route answers.
#[test]
fn refused_edits() {
    let data = "config-version: 8\nserver: {port: 8317}\nplugins: {auth-revision: 3}\n";
    let deep = vec!["a"; MAX_DEPTH + 1].join("/");
    let cases = [
        (edit(V8Method::Put, "", "{bad"), V8EditError::InvalidJson),
        (edit(V8Method::Patch, "", ""), V8EditError::InvalidJson),
        (put_yaml(""), V8EditError::InvalidBody),
        (put_yaml("a: [\n"), V8EditError::InvalidBody),
        (
            edit(V8Method::Put, "", "[1]"),
            V8EditError::ConfigMustBeObject,
        ),
        (
            edit(V8Method::Patch, "", "null"),
            V8EditError::ConfigMustBeObject,
        ),
        (put_yaml("- a\n"), V8EditError::ConfigMustBeObject),
        (
            edit(V8Method::Put, "server//port", "1"),
            V8EditError::InvalidPath,
        ),
        (
            edit(V8Method::Put, "server/port/x", "1"),
            V8EditError::InvalidPath,
        ),
        (edit(V8Method::Put, &deep, "1"), V8EditError::InvalidPath),
        (
            edit(V8Method::Patch, "", r#"{"port":9000}"#),
            V8EditError::InvalidConfig(
                "legacy field port is not accepted by v8; use server.port".to_owned(),
            ),
        ),
        (
            edit(V8Method::Put, "bogus-section", "true"),
            V8EditError::InvalidConfig("unknown v8 configuration section bogus-section".to_owned()),
        ),
        (
            edit(V8Method::Put, "api-keys/bogus", "[]"),
            V8EditError::InvalidConfig("unknown API-key provider bogus".to_owned()),
        ),
        (
            put_yaml("server: {port: 1, port: 2}\n"),
            V8EditError::InvalidConfig(
                "yaml: unmarshal errors:\n  line 1: mapping key \"port\" already defined at line 1"
                    .to_owned(),
            ),
        ),
        (
            edit(
                V8Method::Patch,
                "",
                r#"{"oauth":{"providers":{"codex":null}}}"#,
            ),
            V8EditError::Unprocessable("oauth.providers.codex must be a mapping".to_owned()),
        ),
        (
            edit(V8Method::Put, "plugins/auth-revision", "5"),
            V8EditError::ReadOnlyField("plugins/auth-revision".to_owned()),
        ),
        (
            edit(V8Method::Put, "plugins/auth-revision", r#""3""#),
            V8EditError::ReadOnlyField("plugins/auth-revision".to_owned()),
        ),
        (
            edit(V8Method::Put, "plugins", "{}"),
            V8EditError::ReadOnlyField("plugins/auth-revision".to_owned()),
        ),
        (
            edit(
                V8Method::Put,
                "credentials/concurrency/lifecycle-config-revision",
                "1",
            ),
            V8EditError::ReadOnlyField(
                "credentials/concurrency/lifecycle-config-revision".to_owned(),
            ),
        ),
    ];
    for (edit, error) in cases {
        let got = rendered(data, &edit);
        assert_eq!(got, Err(error), "{edit:?}");
    }
    let got = rendered(data, &edit(V8Method::Put, "server/port", r#""x""#));
    assert!(matches!(got, Err(V8EditError::Unprocessable(_))), "{got:?}");
    // The same value, as the same type, is no change.
    let same = rendered(data, &edit(V8Method::Put, "plugins/auth-revision", "3"));
    assert!(same.is_ok(), "{same:?}");
}

/// A file that doesn't read in the v8 layout refuses every edit.
#[test]
fn stored_invalid_file_refuses_edits() {
    let got = rendered("a: [\n", &edit(V8Method::Put, "server/port", "1"));
    assert!(matches!(got, Err(V8EditError::StoredInvalid(_))), "{got:?}");
}

/// A setting written at an earlier v8 path is saved at its current one.
#[test]
fn historical_paths_move_to_current_ones() {
    let body = r#"{"oauth":{"providers":{"codex":{"response-steering":true,"optimize-multi-agent-v2":true}}}}"#;
    let (out, config) =
        render(b"config-version: 8\n", &edit(V8Method::Patch, "", body)).expect("patch");
    let out = String::from_utf8(out).expect("utf-8");
    assert_valid(&out);
    let root = root_of(&out);
    assert_eq!(
        value_of(&root, "upstream.codex.response-steering"),
        Some("true")
    );
    assert_eq!(
        value_of(&root, "client.codex.optimize-multi-agent-v2"),
        Some("true")
    );
    assert!(yaml_path(&root, "oauth.providers.codex.response-steering").is_none());
    assert!(config.codex.response_steering && config.client.codex.optimize_multi_agent_v2);
}

/// The auth index a read adds to the API-key groups and keys is dropped
/// from a body that carries it back; one in headers is kept. (A file that
/// holds one doesn't load: the loader refuses the field.)
#[test]
fn auth_indexes_are_dropped() {
    let data = "config-version: 8\napi-keys:\n  claude:\n    - name: stored\n      base-url: https://claude.invalid\n      keys:\n        - api-key: sk-stored\n";
    let body = r#"[{"name":"g","base-url":"https://codex.invalid","auth_index":"group","headers":{"auth_index":"kept-header"},"keys":[{"api-key":"sk-a","auth-index":"key"},{"api-key":"sk-b","auth_index":"key"}]}]"#;
    for edit in [
        edit(V8Method::Put, "api-keys/codex", body),
        edit(
            V8Method::Patch,
            "api-keys",
            &format!(r#"{{"codex":{body}}}"#),
        ),
        edit(
            V8Method::Patch,
            "",
            &format!(r#"{{"api-keys":{{"codex":{body}}}}}"#),
        ),
    ] {
        let out = rendered(data, &edit).expect("edit");
        assert_valid(&out);
        for index in ["\"group\"", "\"key\"", ": group", ": key"] {
            assert!(!out.contains(index), "{edit:?}: {index}: {out}");
        }
        assert!(
            out.contains("kept-header") && out.contains("sk-b") && out.contains("sk-stored"),
            "{out}"
        );
    }
}

/// A JSON write of the TURN servers keeps the secrets a JSON read hides,
/// for a server whose URLs are unchanged; a YAML write doesn't.
#[test]
fn turn_secrets_survive_json_writes() {
    let data = "config-version: 8\noauth: {providers: {codex: {live-media-relay: {ice-servers: [{urls: ['turn:a.invalid'], username: user-a, credential: pass-a}, {urls: ['turn:b.invalid'], username: user-b, credential: pass-b}]}}}}\n";
    let path = "oauth/providers/codex/live-media-relay/ice-servers";
    let body =
        r#"[{"urls":["turn:b.invalid"]},{"urls":["turn:a.invalid"]},{"urls":["turn:c.invalid"]}]"#;
    let out = rendered(data, &edit(V8Method::Put, path, body)).expect("put");
    let a = out.find("user-a").expect("user-a");
    let b = out.find("user-b").expect("user-b");
    assert!(b < a, "secrets follow the URLs: {out}");
    assert_eq!(out.matches("pass-").count(), 2, "{out}");

    let yaml = "config-version: 8\noauth: {providers: {codex: {live-media-relay: {ice-servers: [{urls: ['turn:a.invalid']}]}}}}\n";
    let out = rendered(data, &put_yaml(yaml)).expect("yaml");
    assert!(!out.contains("user-a") && !out.contains("pass-a"), "{out}");
}

/// `edit_v8` writes the file it renders and returns the config it loads
/// as; an edit it refuses, or can't write, leaves the file as it was.
#[test]
fn edit_v8_writes_only_what_it_accepts() {
    let dir = TempDir::new();
    let raw = "port: 8317\nrequest-retry: 3\n";
    let file = dir.write("config.yaml", raw);
    let refused = edit_v8(&file, &edit(V8Method::Put, "server/port", r#""x""#));
    assert!(matches!(refused, Err(V8EditError::Unprocessable(_))));
    assert_eq!(fs::read_to_string(&file).expect("read"), raw);

    dir.mkdir("config.yaml.bak");
    let failed = edit_v8(&file, &edit(V8Method::Put, "server/port", "8318"));
    assert!(
        matches!(failed, Err(V8EditError::WriteFailed(_))),
        "{failed:?}"
    );
    assert_eq!(fs::read_to_string(&file).expect("read"), raw);
    fs::remove_dir(dir.join("config.yaml.bak")).expect("remove");

    let config = edit_v8(&file, &edit(V8Method::Put, "server/port", "8318")).expect("edit");
    assert_eq!(config.port, 8318);
    assert!(config == Config::load(&file).expect("load"));
    assert_valid(&fs::read_to_string(&file).expect("read"));

    let missing = edit_v8(
        &dir.join("missing.yaml"),
        &edit(V8Method::Put, "server/port", "1"),
    );
    assert_eq!(missing, Err(V8EditError::ReadFailed));
}

/// An edit's `Debug` shows the body's length only.
#[test]
fn edit_debug_hides_the_body() {
    let edit = edit(
        V8Method::Put,
        "api-keys/codex",
        r#"[{"keys":[{"api-key":"sk-secret"}]}]"#,
    );
    let shown = format!("{edit:?}");
    assert!(
        !shown.contains("sk-secret") && shown.contains("body_len"),
        "{shown}"
    );
}

/// Each error names what the route answers.
#[test]
fn errors_name_the_answer() {
    for (error, shown) in [
        (V8EditError::ReadFailed, "read_failed"),
        (V8EditError::CannotDeleteConfig, "cannot_delete_config"),
        (V8EditError::NotFound, "not_found"),
        (V8EditError::InvalidBody, "invalid_body"),
        (V8EditError::InvalidJson, "invalid_json"),
        (V8EditError::ConfigMustBeObject, "config_must_be_object"),
        (V8EditError::InvalidPath, "invalid_path"),
        (
            V8EditError::InvalidConfig("bad".to_owned()),
            "invalid_config: bad",
        ),
        (
            V8EditError::Unprocessable("bad".to_owned()),
            "invalid_config: bad",
        ),
        (
            V8EditError::StoredInvalid("bad".to_owned()),
            "invalid_config: bad",
        ),
        (
            V8EditError::ReadOnlyField("plugins/auth-revision".to_owned()),
            "read_only_field: plugins/auth-revision",
        ),
        (
            V8EditError::WriteFailed("disk".to_owned()),
            "write_failed: disk",
        ),
    ] {
        assert_eq!(error.to_string(), shown);
    }
}

// Ports the ValidateV8Config checks of TestV8ExampleLoadsAndRoundTrips
// (config_v8_test.go): the example config, and with its provider examples
// uncommented, is a valid v8 config. (Its loads are checked in
// `super::super::load`.)
#[test]
fn v8_example_is_valid() {
    let example = include_str!("../testdata/config.example.yaml");
    assert_valid(example);
    let text = example.replace("\r\n", "\n");
    let (_, block) = text
        .split_once("# BEGIN API KEY EXAMPLES\n")
        .expect("examples start");
    let (block, _) = block
        .split_once("# END API KEY EXAMPLES")
        .expect("examples end");
    let mut uncommented = String::new();
    for line in block
        .trim_end_matches('\n')
        .split('\n')
        .filter(|line| !line.is_empty())
    {
        let line = line.strip_prefix('#').expect("examples stay commented");
        uncommented.push_str(line.strip_prefix(' ').unwrap_or(line));
        uncommented.push('\n');
    }
    assert_valid(&format!("{text}\n{uncommented}"));
}

// Ports TestV8ValidationRejectsLegacyWriteLayout (config_v8_test.go).
#[test]
fn v8_validation_rejects_legacy_write_layout() {
    for raw in [
        "debug: true",
        "server: {port: 8317}\nport: 8318",
        "api-keys: [client]",
        "codex-api-key: []",
        "codex: {}",
        "quota-exceeded: {antigravity-credits: true}",
        "home: {enabled: true}",
        "enable-gemini-cli-endpoint: true",
        "unknown-root: true",
        "<<: {debug: true}",
    ] {
        Config::parse(raw.as_bytes()).expect("legacy file compatibility");
        assert!(
            validate_v8_config(raw.as_bytes()).is_err(),
            "v8 API accepted the legacy write layout: {raw}"
        );
    }
}

/// `path` (dotted) set to `true`, as YAML.
fn nested_true(path: &str) -> String {
    let parts: Vec<&str> = path.split('.').collect();
    let mut text = "true".to_owned();
    for part in parts.iter().rev() {
        text = format!("{{{part}: {text}}}");
    }
    let text = text
        .strip_prefix('{')
        .and_then(|text| text.strip_suffix('}'))
        .expect("braces");
    format!("{text}\n")
}

// Ports the ValidateV8Config checks of TestClientCodexEnableApplyPatch
// (client_test.go), TestClientCodexOptimizeMultiAgentV2 and
// TestClientCodexOptimizeMultiAgentV2Migration (client_optimize_test.go):
// the client settings validate at their current path, as written and once
// moved into the v8 layout, and are refused at a historical one, with an
// unknown key or a repeated one. The values of the wrong type upstream's
// check refuses are refused by the load an edit makes first (see
// `validate`), so they are checked through an edit.
#[test]
fn client_settings_validate_at_their_v8_path() {
    for raw in [
        "config-version: 8\n",
        "client: {codex: {enable-apply-patch: false}}\n",
        "client: {codex: {enable-apply-patch: true}}\n",
        "client: {codex: {optimize-multi-agent-v2: false}}\n",
        "client: {codex: {optimize-multi-agent-v2: true, enable-apply-patch: true}}\n",
        "client: {codex: {optimize-multi-agent-v2: null}}\n",
    ] {
        assert_valid(raw);
        let (migrated, _) = normalize_config_layout(raw.as_bytes(), true).expect("normalize");
        assert_valid(std::str::from_utf8(&migrated).expect("utf-8"));
    }
    for raw in [
        "codex: {optimize-multi-agent-v2: true}\n",
        "providers: {codex: {optimize-multi-agent-v2: true}}\n",
        "oauth: {providers: {codex: {optimize-multi-agent-v2: true}}}\n",
        "client: &client {codex: {optimize-multi-agent-v2: false}}\nproviders: {<<: *client}\n",
    ] {
        let (migrated, _) = normalize_config_layout(raw.as_bytes(), true).expect("normalize");
        assert_valid(std::str::from_utf8(&migrated).expect("utf-8"));
    }
    for raw in [
        "client: {codex: {unknown: true}}\n",
        "client: {codex: {optimize-multi-agent-v2: true, optimize-multi-agent-v2: false}}",
    ] {
        assert!(
            validate_v8_config(raw.as_bytes()).is_err(),
            "accepted {raw}"
        );
    }
    for &(old, _) in V8_CLIENT_PATHS {
        let raw = nested_true(old);
        assert!(
            validate_v8_config(raw.as_bytes()).is_err(),
            "v8 write accepted historical path {old}: {raw}"
        );
    }
    for raw in [
        "client: {codex: {enable-apply-patch: invalid}}\n",
        "client: {codex: {optimize-multi-agent-v2: invalid}}",
        "client: {codex: {optimize-multi-agent-v2: 1}}",
        "client: false",
        "client: {codex: false}",
    ] {
        let got = rendered("config-version: 8\n", &put_yaml(raw));
        assert!(
            matches!(got, Err(V8EditError::Unprocessable(_))),
            "{raw}: {got:?}"
        );
    }
}

// Ports the ValidateV8Config half of TestModelCatalogConfigValidation
// (model_catalogs_test.go): a relative path or another scheme is refused
// with upstream's message, and no sources, an empty source or an https
// source pass. A source of the wrong type is refused too.
#[test]
fn model_catalog_sources_validate() {
    for field in ["catalog", "codex-catalog", "devin-catalog"] {
        for source in [
            "relative.json",
            "./models.json",
            "~/models.json",
            "ftp://example.com/models",
            "file:///tmp/models.json",
            "https:///models",
        ] {
            let raw = format!("models:\n  {field}: {source:?}\n");
            let error = validate_v8_config(raw.as_bytes()).unwrap_err();
            assert_eq!(error.kind(), crate::config::ConfigErrorKind::Invalid);
            assert_eq!(
                error.to_string(),
                format!("models.{field} must be an http(s) URL or an absolute local path")
            );
        }
    }
    for raw in [
        "models: {}",
        "models: {catalog: ''}",
        "models: {codex-catalog: 'https://example.com/models.json'}",
    ] {
        assert_valid(raw);
    }
    assert!(validate_v8_config(b"models: {catalog: [a]}\n").is_err());
}
