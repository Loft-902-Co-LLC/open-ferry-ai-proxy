//! Hand-written cases for the config writer: legacy and v8 files with
//! comments in every position, each quoting and collection style, anchors
//! and aliases, sections open-ferry doesn't type, and the scenarios of
//! upstream's save tests, each saved unchanged, saved with changes, and
//! migrated to the v8 layout; nested scalar updates; and management
//! writes of whole files. Upstream's own `config.example.yaml` is read from
//! the checkout and put through each writer, and so is ours.

use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use super::Case;
use super::config_diff::{ROOT_EXAMPLE, uncomment_root_example};

fn case(name: &str, file: &str, steps: Vec<Value>) -> Case {
    Case::new(name, "", "").with_options(json!({ "file": file, "steps": steps }))
}

/// A save of the file as it stands, as a management write that changes
/// nothing does.
fn resave(migrate: bool) -> Value {
    json!({ "op": "save", "migrate": migrate })
}

/// A save of `config` over the file.
fn save(config: &str, migrate: bool) -> Value {
    json!({ "op": "save", "config": config, "migrate": migrate })
}

fn nested(keys: &[&str], value: &str) -> Value {
    json!({ "op": "nested", "keys": keys, "value": value })
}

fn write(body: &str) -> Value {
    json!({ "op": "write", "body": body })
}

/// A legacy file with comments in every position and sections open-ferry
/// doesn't type.
const LEGACY: &str = r#"# Document head comment.
# Second line of it.

# Server port
port: 8317 # the port

# Where credentials live.
auth-dir: '~/.cli-proxy-api'

debug: false
logging-to-file: false
request-retry: 3
proxy-url: "http://proxy.example:3128"

api-keys:
  # the first key
  - "client-key-1" # one
  - client-key-2
  # after the keys

remote-management:
  allow-remote: false # remote off
  secret-key: ""
  disable-control-panel: false

quota-exceeded:
  switch-project: true
  switch-preview-model: true

routing:
  strategy: round-robin

gemini-api-key:
  - api-key: "gemini-key-1"
    base-url: "https://generativelanguage.googleapis.com"
    # headers for the first key
    headers:
      X-Team: a
  - api-key: gemini-key-2
    prefix: team

claude-api-key:
  - api-key: "claude-key-1" # the claude key
    models:
      - name: "claude-sonnet-4-6"
        alias: "sonnet"

openai-compatibility:
  - name: "local"
    base-url: "http://127.0.0.1:9000/v1"
    api-key-entries:
      - api-key: "local-key"
    models:
      - name: "m1"
        alias: "a1"

oauth-excluded-models:
  codex:
    - "gpt-5.5-mini"

# Plugins aren't typed by open-ferry.
plugins:
  enabled: false
  # a plugin
  configs:
    example:
      enabled: true
      settings: {a: 1, b: [x, y]}

pprof:
  enable: false
  addr: "127.0.0.1:8316"

models:
  catalog: ""

unknown-section:
  a: 1 # kept
  b: [1, 2]

# Document foot comment.
"#;

/// [`LEGACY`] with values changed, lists grown and shrunk, and keys added
/// and removed. (Its `plugins` are the file's on upstream's side: see
/// `go/parity_config_save.go`.)
const LEGACY_CHANGED: &str = r#"port: 8318
auth-dir: "~/.cli-proxy-api"
debug: true
logging-to-file: false
request-retry: 5
max-retry-interval: 30
api-keys:
  - client-key-1
  - client-key-2
  - client-key-3
remote-management:
  allow-remote: true
  secret-key: ""
  disable-control-panel: false
quota-exceeded:
  switch-project: false
  switch-preview-model: true
routing:
  strategy: fill-first
gemini-api-key:
  - api-key: gemini-key-2
    prefix: team
claude-api-key:
  - api-key: claude-key-1
    models:
      - name: claude-sonnet-4-6
        alias: sonnet
      - name: claude-opus-4-6
        alias: opus
  - api-key: claude-key-2
codex-api-key:
  - api-key: codex-key-1
    base-url: https://api.openai.com/v1
openai-compatibility:
  - name: local
    base-url: http://127.0.0.1:9000/v1
    api-key-entries:
      - api-key: local-key
      - api-key: local-key-2
    models:
      - name: m2
        alias: a2
"#;

/// A v8 file with comments in every position, after upstream's
/// TestV8MovedCommentsSurviveV0Saves.
const V8: &str = r#"# DOCUMENT HEAD
config-version: 8
oauth: # OAUTH INLINE
  providers: # PROVIDERS INLINE
    xai: # PROVIDER INLINE
      # FIELD HEAD
      inject-x-search: true # FIELD INLINE

      # FIELD FOOT

    # PROVIDER FOOT
server:
  port: 8317 # UNRELATED INLINE
# DOCUMENT FOOT
"#;

/// A fuller v8 file: providers' keys grouped, client keys, routing, and
/// sections open-ferry doesn't type.
const V8_FULL: &str = r#"# v8 layout
config-version: 8

server:
  host: ""
  # the port
  port: 8317
  tls:
    enable: false

access:
  api-keys:
    - client-key-1 # first
    - client-key-2

management:
  allow-remote: false
  secret-key: ""

requests:
  proxy-url: ""
  request-retry: 2

routing:
  strategy: round-robin

api-keys:
  gemini:
    - name: gemini-1
      # a padded prefix
      prefix: " team "
      keys:
        - api-key: gemini-key-1
        - api-key: gemini-key-2 # second
  claude:
    - keys:
        - api-key: claude-key-1
      models:
        - name: claude-sonnet-4-6
          alias: sonnet
  codex:
    - keys: [{api-key: codex-key-1}]
  openai-compatibility:
    - name: local
      base-url: http://127.0.0.1:9000/v1
      keys:
        - api-key: local-key

oauth:
  excluded-models:
    codex:
      - gpt-5.5-mini

observability:
  pprof:
    enable: false
    addr: 127.0.0.1:8316

plugins:
  enabled: false

models:
  catalog: ""
# end
"#;

/// Scalars in every quoting style, block scalars among them.
const STYLES: &str = r#"port: 8317
auth-dir: '~/.cli-proxy-api'
proxy-url: "http://proxy.example:3128"
'debug': false
"request-retry": 3
api-keys:
  - 'single ''quoted'''
  - "double \"quoted\" é"
  - plain key
  - |
    literal
    block
  - >
    folded
    block
  - "-starts-with-dash"
  - "123"
  - "true"
  - "null"
  - ""
unknown-styles:
  literal: |-
    keep
    this
  folded: >+
    keep

  quoted: 'it''s'
  number: "8317"
  bool: 'false'
"#;

/// Anchors and aliases, the merge key among them.
const ANCHORS: &str = r#"port: &port 8317
request-retry: *port
api-keys: &keys
  - client-key-1
  - client-key-2
base-headers: &headers
  X-Team: a
  X-Other: b
gemini-api-key:
  - api-key: gemini-key-1
    headers: *headers
  - api-key: gemini-key-2
    headers:
      <<: *headers
      X-Team: c
unknown-section:
  keys: *keys
"#;

/// Flow collections, nested and empty.
const FLOW: &str = r#"{port: 8317, debug: false, api-keys: [client-key-1, "client-key-2"], routing: {strategy: round-robin}, gemini-api-key: [{api-key: gemini-key-1, headers: {X-Team: a}}], unknown: {a: [], b: {}}}
"#;

const FLOW_BLOCK: &str = r#"port: 8317
api-keys: [client-key-1, client-key-2]
routing: {strategy: round-robin}
oauth-excluded-models: {codex: [gpt-5.5-mini, "claude-2*"]}
gemini-api-key: [{api-key: gemini-key-1}, {api-key: gemini-key-2, prefix: team}]
trusted-proxies: []
unknown: {}
"#;

/// Sequences written without an indent under their key, and a four-space
/// indent.
const INDENTS: &str = r#"port: 8317
api-keys:
- client-key-1
- client-key-2
gemini-api-key:
-   api-key: gemini-key-1
    headers:
        X-Team: a
remote-management:
    allow-remote: false
    secret-key: ""
"#;

/// Comments in odd places: before the first key with no blank line,
/// between a key and its block, after the last item, indented wrongly, and
/// at the very end with no newline.
const ODD_COMMENTS: &str = "port: 8317\n# between\napi-keys: # on the key\n  # before the first item\n  - client-key-1\n      # indented too far\n  - client-key-2\n# below the list\n\n\n# after blank lines\nrouting:\n    # deeper than its key\n  strategy: round-robin\n# last, no newline";

/// Legacy and v8 spellings of the same settings.
const MIXED: &str = r#"port: 8317
request-retry: 3
server:
  port: 8318
requests:
  request-retry: 4
"#;

/// Upstream's TestV0SaveUpgradesHistoricalV8Layout fixture.
const HISTORICAL_V8: &str = "# Preserve provider settings\noauth: {providers: {codex: {response-steering: true, header-defaults: {user-agent: oauth-agent}}, xai: {inject-x-search: true}}}\n";

/// The four OAuth maps in the legacy layout, with comments.
const OAUTH: &str = r#"# OAuth maps.
port: 8317
oauth-excluded-models: # excluded
  # codex head
  codex:
    - gpt-5-codex-mini # first
    - "gpt-5.5-*"
  claude:
    - claude-3-5-haiku-20241022
oauth-model-alias:
  codex:
    - name: gpt-5 # the model
      alias: g5
    - name: gpt-5.5
      alias: g55
      fork: true
  claude:
    - name: claude-sonnet-4-5-20250929
      alias: cs4.5
oauth-request-scoped-errors:
  codex:
    - status: 400
      match:
        - context_window_exceeded # matched
      action: stop
oauth-settings:
  codex:
    - name: gpt-6-sol
      max-context-length: 524288 # window
# after the maps
"#;

/// The OAuth maps of [`OAUTH`], changed: providers added and removed,
/// entries added, removed, reordered and changed.
const OAUTH_CHANGED: &str = r#"port: 8317
oauth-excluded-models:
  codex:
    - "gpt-5.5-*"
    - gpt-4o
  xai:
    - grok-3-mini
oauth-model-alias:
  codex:
    - name: gpt-5.5
      alias: g55
    - name: gpt-5
      alias: g5x
  vertex:
    - name: gemini-2.5-pro
      alias: g2.5p
oauth-request-scoped-errors:
  codex:
    - status: 400
      match:
        - context_window_exceeded
        - maximum_context_length
      action: continue
  claude:
    - status: 400
      match:
        - prompt is too long
      action: stop
oauth-settings:
  codex:
    - name: gpt-6-sol
      max-context-length: 262144
    - name: gpt-6-astra
      max-context-length: 1048576
"#;

/// [`OAUTH`] in the v8 layout.
const OAUTH_V8: &str = r#"config-version: 8
# OAuth maps.
server:
  port: 8317
oauth:
  excluded-models: # excluded
    # codex head
    codex:
      - gpt-5-codex-mini # first
      - "gpt-5.5-*"
    claude:
      - claude-3-5-haiku-20241022
  model-alias:
    codex:
      - name: gpt-5 # the model
        alias: g5
      - name: gpt-5.5
        alias: g55
        fork: true
  request-scoped-errors:
    codex:
      - status: 400
        match:
          - context_window_exceeded # matched
        action: stop
  settings:
    codex:
      - name: gpt-6-sol
        max-context-length: 524288 # window
# after the maps
"#;

/// [`OAUTH_CHANGED`] in the v8 layout.
const OAUTH_V8_CHANGED: &str = r#"server:
  port: 8317
oauth:
  excluded-models:
    codex: ["gpt-5.5-*", gpt-4o]
    xai: [grok-3-mini]
  model-alias:
    codex:
      - {name: gpt-5.5, alias: g55}
      - {name: gpt-5, alias: g5x}
    vertex:
      - {name: gemini-2.5-pro, alias: g2.5p}
  request-scoped-errors:
    codex:
      - {status: 400, match: [context_window_exceeded, maximum_context_length], action: continue}
  settings:
    codex:
      - {name: gpt-6-sol, max-context-length: 262144}
"#;

/// Unknown v8 sections and fields, which a migration comments out.
const V8_UNKNOWN: &str = r#"config-version: 8
server:
  port: 8317
  bogus-field: 1 # a field v8 doesn't know
mystery:
  # a section v8 doesn't know
  a: 1
  b:
    - x
oauth:
  providers:
    codex:
      not-a-field: true
"#;

/// Plugin configs under quoted IDs out of order.
const PLUGINS_UNSORTED: &str = r#"port: 8317
plugins:
  enabled: false
  configs:
    "gamma":
      enabled: true
    # the alpha plugin
    alpha:
      enabled: false
"#;

/// A plugin's settings ending the file with a comment at their end.
const PLUGINS_FOOT_COMMENT: &str = r#"port: 8317
plugins:
  configs:
    beta:
      settings:
        tags:
          - a
        # the end of beta's settings
"#;

const PLUGINS_REWRITTEN: &str = "upstream writes the plugins section from the config it decoded; \
     open-ferry, with no plugin host, keeps the file's";

pub fn steps(upstream: &Path) -> Vec<Case> {
    let mut cases = vec![
        case("legacy-resave", LEGACY, vec![resave(false)]),
        case("legacy-change", LEGACY, vec![save(LEGACY_CHANGED, false)]),
        case("legacy-migrate", LEGACY, vec![resave(true)]),
        case(
            "legacy-migrate-change",
            LEGACY,
            vec![resave(true), save(LEGACY_CHANGED, false), resave(false)],
        ),
        case(
            "legacy-change-migrate",
            LEGACY,
            vec![save(LEGACY_CHANGED, true)],
        ),
        case(
            "legacy-shrink-to-nothing",
            LEGACY,
            vec![save("port: 8317\n", false)],
        ),
        case("v8-resave", V8, vec![resave(false), resave(false)]),
        case(
            "v8-port-saves",
            V8,
            vec![
                save(
                    "server:\n  port: 8318\noauth:\n  providers:\n    xai:\n      inject-x-search: true\n",
                    false,
                ),
                save(
                    "server:\n  port: 8319\noauth:\n  providers:\n    xai:\n      inject-x-search: true\n",
                    false,
                ),
                save("port: 8320\nxai:\n  inject-x-search: true\n", false),
            ],
        ),
        case("v8-migrate", V8, vec![resave(true)]),
        case("v8-full-resave", V8_FULL, vec![resave(false)]),
        case("v8-full-change", V8_FULL, vec![save(LEGACY_CHANGED, false)]),
        case("v8-full-migrate", V8_FULL, vec![save(LEGACY_CHANGED, true)]),
        case("styles-resave", STYLES, vec![resave(false)]),
        case(
            "styles-change",
            STYLES,
            vec![save(
                "port: 8318\napi-keys: [\"single 'quoted'\", plain key, new key]\n",
                false,
            )],
        ),
        case("styles-migrate", STYLES, vec![resave(true)]),
        case("anchors-resave", ANCHORS, vec![resave(false)]),
        case(
            "anchors-change",
            ANCHORS,
            vec![save(
                "port: 8318\nrequest-retry: 8317\napi-keys: [client-key-1]\ngemini-api-key:\n  - api-key: gemini-key-1\n    headers: {X-Team: a, X-Other: b}\n",
                false,
            )],
        ),
        case("anchors-migrate", ANCHORS, vec![resave(true)]),
        case("flow-resave", FLOW, vec![resave(false)]),
        case(
            "flow-change",
            FLOW,
            vec![save(
                "port: 8318\napi-keys: [client-key-2, client-key-3]\ngemini-api-key: [{api-key: gemini-key-1}]\n",
                false,
            )],
        ),
        case("flow-migrate", FLOW, vec![resave(true)]),
        case("flow-block-resave", FLOW_BLOCK, vec![resave(false)]),
        case(
            "flow-block-change",
            FLOW_BLOCK,
            vec![save(
                "port: 8317\napi-keys: [client-key-1, client-key-2, client-key-3]\noauth-excluded-models: {codex: [gpt-5.5-mini]}\ngemini-api-key: [{api-key: gemini-key-2, prefix: team}]\ntrusted-proxies: [127.0.0.1]\n",
                false,
            )],
        ),
        case("indents-resave", INDENTS, vec![resave(false)]),
        case(
            "indents-change",
            INDENTS,
            vec![save(
                "port: 8317\napi-keys: [client-key-1, client-key-2, client-key-3]\ngemini-api-key:\n  - api-key: gemini-key-1\n    headers: {X-Team: b}\n  - api-key: gemini-key-2\n",
                false,
            )],
        ),
        case("odd-comments-resave", ODD_COMMENTS, vec![resave(false)]),
        case(
            "odd-comments-change",
            ODD_COMMENTS,
            vec![save("port: 8317\napi-keys: [client-key-2]\n", false)],
        ),
        case("odd-comments-migrate", ODD_COMMENTS, vec![resave(true)]),
        case("mixed-resave", MIXED, vec![resave(false)]),
        case("mixed-migrate", MIXED, vec![resave(true)]),
        case(
            "historical-v8-port",
            HISTORICAL_V8,
            vec![save(&format!("{HISTORICAL_V8}port: 8318\n"), false)],
        ),
        case(
            "declared-historical-v8-port",
            &format!("config-version: 8\n{HISTORICAL_V8}"),
            vec![resave(false)],
        ),
        case("v8-unknown-resave", V8_UNKNOWN, vec![resave(false)]),
        case("v8-unknown-migrate", V8_UNKNOWN, vec![resave(true)]),
        case("empty-file", "", vec![save("port: 8317\n", false)]),
        case(
            "comment-only-file",
            "# nothing yet\n",
            vec![save("port: 8317\n", false)],
        ),
        case("list-root", "- a\n- b\n", vec![save("port: 8317\n", false)]),
        case("bad-yaml", "port: [\n", vec![save("port: 8317\n", false)]),
        case(
            "nested-legacy",
            LEGACY,
            vec![nested(
                &["remote-management", "secret-key"],
                "$2a$10$abcdefghijklmnopqrstuv",
            )],
        ),
        case(
            "nested-v8",
            V8_FULL,
            vec![nested(
                &["management", "secret-key"],
                "$2a$10$abcdefghijklmnopqrstuv",
            )],
        ),
        case(
            "nested-new-path",
            LEGACY,
            vec![nested(&["new-section", "inner", "leaf"], "value")],
        ),
        // Go writes a file its own LoadConfig refuses; open-ferry refuses
        // the write (counted as equivalent).
        case(
            "nested-through-scalar",
            LEGACY,
            vec![nested(&["port", "inner"], "value")],
        ),
        case(
            "nested-alias",
            ANCHORS,
            vec![nested(&["base-headers", "X-Team"], "z")],
        ),
        case("nested-empty-file", "", vec![nested(&["a", "b"], "c")]),
        case("nested-list-root", "- a\n", vec![nested(&["a"], "c")]),
        case(
            "nested-flow",
            FLOW,
            vec![nested(&["routing", "strategy"], "fill-first")],
        ),
        case("write-legacy", "", vec![write(LEGACY)]),
        case("write-v8", "", vec![write(V8)]),
        case("write-v8-full", "", vec![write(V8_FULL)]),
        case("write-v8-unknown", "", vec![write(V8_UNKNOWN)]),
        case("write-odd-comments", "", vec![write(ODD_COMMENTS)]),
        case("write-historical-v8", "", vec![write(HISTORICAL_V8)]),
        case("write-mixed", "", vec![write(MIXED)]),
        case("write-bad-yaml", "", vec![write("port: [\n")]),
        case(
            "write-then-save",
            "",
            vec![write(V8_FULL), save(LEGACY_CHANGED, false), resave(false)],
        ),
        case("oauth-resave", OAUTH, vec![resave(false)]),
        case("oauth-change", OAUTH, vec![save(OAUTH_CHANGED, false)]),
        case(
            "oauth-change-migrate",
            OAUTH,
            vec![save(OAUTH_CHANGED, true)],
        ),
        case(
            "oauth-migrate-change",
            OAUTH,
            vec![resave(true), save(OAUTH_CHANGED, false)],
        ),
        case(
            "oauth-change-back",
            OAUTH,
            vec![save(OAUTH_CHANGED, false), save(OAUTH, false)],
        ),
        // Emptying a map whose key has a line comment: Go writes `{}` on the
        // next line at column 0, which its own LoadConfig refuses; open-ferry
        // refuses the write (counted as equivalent).
        case("oauth-clear", OAUTH, vec![save("port: 8317\n", false)]),
        case(
            "oauth-clear-uncommented",
            &OAUTH.replace(
                "oauth-excluded-models: # excluded\n",
                "oauth-excluded-models:\n",
            ),
            vec![save("port: 8317\n", false)],
        ),
        case(
            "oauth-clear-one-provider",
            OAUTH,
            vec![save(
                &OAUTH
                    .replace("  claude:\n    - claude-3-5-haiku-20241022\n", "")
                    .replace(
                        "  claude:\n    - name: claude-sonnet-4-5-20250929\n      alias: cs4.5\n",
                        "",
                    ),
                false,
            )],
        ),
        case(
            "oauth-legacy-config-over-v8",
            OAUTH_V8,
            vec![save(OAUTH_CHANGED, false)],
        ),
        case("oauth-v8-resave", OAUTH_V8, vec![resave(false)]),
        case(
            "oauth-v8-change",
            OAUTH_V8,
            vec![save(OAUTH_V8_CHANGED, false)],
        ),
        case(
            "oauth-v8-change-migrate",
            OAUTH_V8,
            vec![save(OAUTH_V8_CHANGED, true)],
        ),
        case(
            "oauth-v8-clear",
            OAUTH_V8,
            vec![save("server: {port: 8317}\n", false)],
        ),
        case(
            "oauth-v8-change-back",
            OAUTH_V8,
            vec![save(OAUTH_V8_CHANGED, false), save(OAUTH_V8, false)],
        ),
        case("oauth-write-v8", "", vec![write(OAUTH_V8)]),
        // Upstream writes the comment at the end of a plugin's settings that
        // end the file twice, from the plugin's decoded node and from the
        // file; open-ferry keeps the file's plugins, so writes it once
        // (counted as equivalent).
        case(
            "plugins-foot-comment",
            PLUGINS_FOOT_COMMENT,
            vec![resave(false)],
        ),
        case(
            "plugins-foot-comment-migrate",
            PLUGINS_FOOT_COMMENT,
            vec![resave(true), resave(false)],
        ),
        // Upstream writes `plugins.dir` as its ResolvePluginsDir cleaned it.
        case(
            "plugins-dir-cleaned",
            "port: 8317\nplugins:\n  enabled: false\n  dir: ./plugins/\n",
            vec![resave(false)],
        )
        .known_difference(PLUGINS_REWRITTEN),
        // Upstream writes the plugin IDs from a map: sorted and plain.
        case(
            "plugins-configs-rebuilt",
            PLUGINS_UNSORTED,
            vec![save("port: 8318\n", false)],
        )
        .known_difference(PLUGINS_REWRITTEN),
    ];
    if let Ok(example) = fs::read_to_string(upstream.join("config.example.yaml")) {
        let keys = uncomment_api_keys(&example);
        cases.extend([
            case("example-resave", &example, vec![resave(false)]),
            case("example-migrate", &example, vec![resave(true)]),
            case(
                "example-change",
                &example,
                vec![save(
                    &example.replacen("port: 8317", "port: 8318", 1).replacen(
                        "debug: false",
                        "debug: true",
                        1,
                    ),
                    false,
                )],
            ),
            case("example-keys-resave", &keys, vec![resave(false)]),
            case("example-keys-migrate", &keys, vec![resave(true)]),
            case("example-keys-write", "", vec![write(&keys)]),
            case(
                "example-keys-change",
                &keys,
                vec![save(
                    &keys.replacen("\"AIzaSy...01\"", "\"AIzaSy...09\"", 1),
                    false,
                )],
            ),
            case("example-write", "", vec![write(&example)]),
            case(
                "example-nested",
                &example,
                vec![nested(
                    &["management", "secret-key"],
                    "$2a$10$abcdefghijklmnopqrstuv",
                )],
            ),
        ]);
    }
    // Not upstream's: the root config.example.yaml, ours, put through each
    // writer as upstream's is.
    let uncommented = uncomment_root_example();
    cases.extend([
        case("root-example-resave", ROOT_EXAMPLE, vec![resave(false)]),
        case("root-example-migrate", ROOT_EXAMPLE, vec![resave(true)]),
        case("root-example-write", "", vec![write(ROOT_EXAMPLE)]),
        case(
            "root-example-uncommented-resave",
            &uncommented,
            vec![resave(false)],
        ),
        case(
            "root-example-uncommented-write",
            "",
            vec![write(&uncommented)],
        ),
    ]);
    cases
}

/// Upstream's example with its commented-out `api-keys` section, the v8
/// layout's provider keys, taken out of comments.
fn uncomment_api_keys(example: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in example.split_inclusive('\n') {
        if line.starts_with("# api-keys:") {
            inside = true;
        } else if inside && !line.starts_with('#') {
            inside = false;
        }
        if inside {
            let rest = line.strip_prefix('#').unwrap_or(line);
            out.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else {
            out.push_str(line);
        }
    }
    out
}
