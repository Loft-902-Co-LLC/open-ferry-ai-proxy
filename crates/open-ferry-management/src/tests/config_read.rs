// Ported from CLIProxyAPI internal/api/handlers/management/
// config_openai_compat_test.go (TestGetOpenAICompatIncludesDisableCooling),
// config_v8_test.go (TestConfigV8MigrationAndLegacyAPI,
// TestConfigV8JSONTURNSecrets), config_v8_client_test.go
// (TestConfigV8ClientMultiAgentMigration) and
// internal/api/server_management_v8_test.go
// (TestManagementV8RoutesShareAccessControl,
// TestManagementV8IndependentContract) (v8.0.10, MIT), and
// config_v8_compatibility_test.go (TestConfigV8HistoricalFieldPaths,
// TestConfigV8HistoricalProviderSubtrees) (v8.0.11, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the routes of `crate::config_read`.
//!
//! The expected bodies are upstream's answers to the same files, less the
//! sections open-ferry doesn't type.
//!
//! Deviations from upstream:
//! - Upstream's tests write the config through these routes and read it
//!   back. Only their reads are kept: each write answers with the empty 404
//!   here and leaves the file as it was. So
//!   `TestConfigV8MigrationAndLegacyAPI`,
//!   `TestConfigV8ClientMultiAgentMigration`,
//!   `TestConfigV8HistoricalFieldPaths` and
//!   `TestConfigV8HistoricalProviderSubtrees` read a file that already holds
//!   what upstream's writes put there, and `TestConfigV8JSONTURNSecrets`
//!   drops its `PUT` round trips.
//! - `TestManagementV8RoutesShareAccessControl` requests only this module's
//!   routes (the credential list is checked in `server_management_v8`), and
//!   drops its Home mode case: Home mode isn't ported.
//! - `TestManagementV8IndependentContract` keeps only its config checks; the
//!   rest is in `server_management_v8`.
//! - The other tests of config_v8_test.go (`TestConfigV8CommentsUnknown*`,
//!   `TestV8NestedWriteMigratesOnlyOnSuccess`,
//!   `TestV8MigrationReloadSnapshotMatchesDisk`,
//!   `TestV8GroupedCredentialsSurviveLegacyWrites`,
//!   `TestConfigV8DeleteLastField`, `TestConfigV8ReplaceEmptyGroup`,
//!   `TestConfigV8EmptyExcludedModelsSurvivesSave`,
//!   `TestConfigV8DeletePreservesDocumentPresence`), the rest of
//!   config_v8_compatibility_test.go, config_v8_upstream_test.go and the
//!   other config_*_test.go files test writes, and are dropped.
//!   (config_basic_weight_test.go is ported in `crate::config_read`.)

use http::{Method, StatusCode};
use open_ferry_core::auth::synthesizer::{
    StableIdGenerator, SynthesisContext, synthesize_config_auths,
};
use open_ferry_core::config::Config;
use serde_json::Value;

use super::{Answer, Api, AuthDir, LOCAL, keyed, keyed_config, request_from};

/// The legacy config of upstream's `TestConfigV8MigrationAndLegacyAPI`.
const LEGACY: &str = "# Keep this configuration\nport: 8317\nrequest-retry: 3\napi-keys: [client]\nws-auth: true\ntls: {}\npayload: null\ncodex: {live-media-relay: {}}\n";

/// [`LEGACY`] in the v8 layout, as upstream writes it.
const LEGACY_V8: &str = concat!(
    r#"{"access":{"api-keys":["client"]},"config-version":8,"oauth":{"providers":"#,
    r#"{"aistudio":{"ws-auth":true},"codex":{"live-media-relay":{}}}},"#,
    r#""requests":{"payload":{}},"routing":{"retry":{"request-retry":3}},"#,
    r#""server":{"port":8317,"tls":{}}}"#,
);

/// A config with something in each section open-ferry types.
const RICH: &str = r#"port: 8317
proxy-url: socks5://127.0.0.1:1080
force-model-prefix: true
request-log: true
api-keys: [k1, k2]
passthrough-headers: true
streaming: {keepalive-seconds: 15, bootstrap-retries: 2}
nonstream-keepalive-interval: 5
trusted-proxies: [10.0.0.0/8]
tls: {enable: true, cert: c.pem, key: k.pem}
debug: true
logging-to-file: true
disable-cooling: true
transient-error-cooldown-seconds: 7
auth-auto-refresh-workers: 3
request-retry: 4
max-retry-credentials: 2
max-retry-interval: 30
quota-exceeded: {switch-project: true, switch-preview-model: true, antigravity-credits: true}
routing: {strategy: RR}
ws-auth: true
gemini-api-key:
  - api-key: g1
    priority: 2
    weight: 3
    prefix: team
    base-url: https://g.example
    proxy-url: http://p.example
    headers: {X-B: b, X-A: a}
    models: [{name: gemini-2.5-pro, alias: gp}]
    excluded-models: [gemini-1*]
    disable-cooling: false
    request-retry: 1
codex-api-key:
  - api-key: c1
    base-url: https://c.example
    websockets: true
    models: [{name: gpt-5, alias: g5}]
claude-api-key:
  - api-key: a1
    models: [{name: claude-x, alias: cx}]
openai-compatibility:
  - name: Mimo CN
    base-url: " https://token-plan-cn.xiaomimimo.com/v1 "
    api-key-entries: [{api-key: test-key, proxy-url: http://q.example}]
    models: [{name: mimo-v2.5}]
    support-prompt-cache-key: true
    disable-cooling: true
    request-retry: 0
  - name: nokeys
    base-url: https://n.example
vertex-api-key:
  - api-key: v1
    base-url: https://v.example
    models: [{name: m, alias: a}]
oauth-excluded-models: {codex: [a, b]}
oauth-model-alias: {codex: [{name: gpt-5, alias: g5, fork: true}]}
oauth-request-scoped-errors: {codex: [{status: 400, match: [bad], action: retry}]}
codex: {stream-bootstrap-buffering: true, model-level-cooling: true}
codex-header-defaults: {beta-features: x}
claude: {model-level-cooling: true}
client: {codex: {optimize-multi-agent-v2: true}}
"#;

/// Upstream's Gemini key list for [`RICH`].
const RICH_GEMINI: &str = concat!(
    r#"[{"api-key":"g1","priority":2,"weight":3,"prefix":"team","#,
    r#""base-url":"https://g.example","proxy-url":"http://p.example","#,
    r#""models":[{"name":"gemini-2.5-pro","alias":"gp"}],"headers":{"X-A":"a","X-B":"b"},"#,
    r#""excluded-models":["gemini-1*"],"disable-cooling":false,"request-retry":1}]"#,
);

/// Upstream's Codex key list for [`RICH`].
const RICH_CODEX: &str = concat!(
    r#"[{"api-key":"c1","base-url":"https://c.example","websockets":true,"#,
    r#""proxy-url":"","models":[{"name":"gpt-5","alias":"g5"}]}]"#,
);

/// Upstream's Claude key list for [`RICH`].
const RICH_CLAUDE: &str = r#"[{"api-key":"a1","base-url":"","proxy-url":"","models":[{"name":"claude-x","alias":"cx"}]}]"#;

/// Upstream's Vertex key list for [`RICH`].
const RICH_VERTEX: &str =
    r#"[{"api-key":"v1","base-url":"https://v.example","models":[{"name":"m","alias":"a"}]}]"#;

/// The API with `config` parsed, and the management key.
fn with_config(text: &str) -> Api {
    let mut config = Config::parse(text).unwrap();
    config.remote_management.secret_key = keyed_config().remote_management.secret_key;
    Api::with(config, None)
}

/// The API over a temporary directory whose config file holds `raw`.
fn over_file(raw: &str) -> (AuthDir, Api) {
    let dir = AuthDir::new();
    std::fs::write(dir.config_path(), raw).unwrap();
    let api = Api::over(&dir);
    (dir, api)
}

/// Checks that the config file still holds `raw`.
fn assert_unchanged(dir: &AuthDir, raw: &str) {
    let data = std::fs::read_to_string(dir.config_path()).unwrap();
    assert_eq!(data, raw, "the config file changed");
}

/// Checks a read that succeeded: `body`, as JSON not to be cached.
fn assert_v8(answer: &Answer, body: &str) {
    answer.assert(StatusCode::OK, body);
    assert_eq!(
        answer.header("content-type"),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(answer.header("cache-control"), Some("no-store"));
}

/// Checks that `method` on `path` answers with the empty 404.
async fn assert_unported(api: &Api, method: Method, path: &str) {
    let answer = api.send(keyed(method.clone(), path, "{}")).await;
    assert_eq!(
        (answer.status, answer.body.as_str()),
        (StatusCode::NOT_FOUND, ""),
        "{method} {path}"
    );
}

/// Ported from upstream's config_v8_test.go
/// (TestConfigV8MigrationAndLegacyAPI): a legacy file reads in the v8
/// layout without being migrated, and the writes are refused.
#[tokio::test]
async fn config_v8_migration_and_legacy_api() {
    let (dir, api) = over_file(LEGACY);
    assert_v8(&api.get("/v8/management/config").await, LEGACY_V8);
    assert_unchanged(&dir, LEGACY);
    assert_unported(&api, Method::PATCH, "/v8/management/config").await;
    assert_unported(&api, Method::PUT, "/v0/management/request-retry").await;
    assert_unported(&api, Method::PUT, "/v8/management/config.yaml").await;
    for path in ["requests/proxy-url", "server/unknown-option"] {
        let path = format!("/v8/management/config/{path}");
        assert_unported(&api, Method::PUT, &path).await;
    }
    assert_unchanged(&dir, LEGACY);
}

/// Not upstream's: a path's segments name mapping keys in the v8 layout,
/// extra slashes don't count, and a legacy name isn't found, as upstream
/// answers the same requests.
#[tokio::test]
async fn config_v8_paths_name_values() {
    let (dir, api) = over_file(LEGACY);
    for (path, body) in [
        ("routing/retry/request-retry", "3"),
        ("server/tls", "{}"),
        ("server/", r#"{"port":8317,"tls":{}}"#),
        ("", LEGACY_V8),
        ("/", LEGACY_V8),
    ] {
        let answer = api.get(&format!("/v8/management/config/{path}")).await;
        assert_v8(&answer, body);
    }
    for path in ["api-keys", "request-retry", "nope", "server/port/x"] {
        let answer = api.get(&format!("/v8/management/config/{path}")).await;
        answer.assert(StatusCode::NOT_FOUND, r#"{"error":"not_found"}"#);
        assert_eq!(answer.header("cache-control"), None, "{path}");
    }
    assert_unchanged(&dir, LEGACY);
}

/// Not upstream's: the v8 `config.yaml` is the file as it is, not to be
/// cached; upstream sends it migrated to the v8 layout.
#[tokio::test]
async fn config_v8_yaml_is_the_file() {
    let (_dir, api) = over_file(LEGACY);
    let answer = api.get("/v8/management/config.yaml").await;
    answer.assert(StatusCode::OK, LEGACY);
    assert_eq!(
        answer.header("content-type"),
        Some("application/yaml; charset=utf-8")
    );
    assert_eq!(answer.header("cache-control"), Some("no-store"));
}

/// Ported from upstream's config_v8_test.go (TestConfigV8JSONTURNSecrets):
/// the JSON reads leave out the ICE servers' usernames and credentials,
/// which `config.yaml` keeps.
#[tokio::test]
async fn config_v8_json_hides_turn_secrets() {
    let raw = "oauth:\n  providers:\n    codex:\n      live-media-relay:\n        ice-servers:\n          - {urls: ['turn:example.invalid:3478'], username: test-relay-user, credential: test-relay-password}\n          - {urls: ['turn:example.invalid:3478'], username: test-relay-user-2, credential: test-relay-password-2}\n";
    let (dir, api) = over_file(raw);
    let servers =
        r#"[{"urls":["turn:example.invalid:3478"]},{"urls":["turn:example.invalid:3478"]}]"#;
    let whole = format!(
        r#"{{"config-version":8,"oauth":{{"providers":{{"codex":{{"live-media-relay":{{"ice-servers":{servers}}}}}}}}}}}"#
    );
    assert_v8(&api.get("/v8/management/config").await, &whole);
    let path = "/v8/management/config/oauth/providers/codex/live-media-relay/ice-servers";
    assert_v8(&api.get(path).await, servers);
    assert_unported(&api, Method::PUT, path).await;
    let answer = api.get("/v8/management/config.yaml").await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(
        answer.body.contains("test-relay-password"),
        "{}",
        answer.body
    );
    assert_unchanged(&dir, raw);
}

/// Ported from upstream's config_v8_client_test.go
/// (TestConfigV8ClientMultiAgentMigration): the Codex client setting reads
/// at its v8 path and at each historical one, wherever the file has it.
#[tokio::test]
async fn config_v8_client_multi_agent_migration() {
    for raw in [
        "codex: {optimize-multi-agent-v2: true}\n",
        "providers: {codex: {optimize-multi-agent-v2: true}}\n",
        "oauth: {providers: {codex: {optimize-multi-agent-v2: true}}}\n",
    ] {
        let raw = format!("{raw}client: {{codex: {{enable-apply-patch: true}}}}\n");
        let (dir, api) = over_file(&raw);
        let canonical = "/v8/management/config/client/codex/optimize-multi-agent-v2";
        assert_v8(&api.get(canonical).await, "true");
        for method in [Method::PUT, Method::DELETE] {
            assert_unported(&api, method, canonical).await;
        }
        for old in ["providers/codex", "oauth/providers/codex", "codex"] {
            let path = format!("/v8/management/config/{old}/optimize-multi-agent-v2");
            assert_v8(&api.get(&path).await, "true");
        }
        assert_v8(
            &api.get("/v8/management/config").await,
            r#"{"client":{"codex":{"enable-apply-patch":true,"optimize-multi-agent-v2":true}},"config-version":8}"#,
        );
        assert_unchanged(&dir, &raw);
    }
}

/// Ported from upstream's config_v8_compatibility_test.go
/// (TestConfigV8HistoricalFieldPaths): a setting reads at its historical
/// path as at its v8 one.
#[tokio::test]
async fn config_v8_historical_field_paths() {
    for (historical, current, value) in [
        (
            "codex/disable-codex-cloaking",
            "codex/disable-codex-cloaking",
            "true",
        ),
        (
            "codex/stream-bootstrap-buffering",
            "codex/stream-bootstrap-buffering",
            "true",
        ),
        (
            "codex/stream-bootstrap-timeout",
            "codex/stream-bootstrap-timeout",
            r#""10s""#,
        ),
        (
            "codex/orphan-delegation-compatibility",
            "codex/orphan-delegation-compatibility",
            "true",
        ),
        (
            "codex/model-level-cooling",
            "codex/model-level-cooling",
            "true",
        ),
        ("codex/response-steering", "codex/response-steering", "true"),
        (
            "claude/model-level-cooling",
            "claude/model-level-cooling",
            "true",
        ),
        (
            "claude/disable-claude-cloak-mode",
            "claude/disable-claude-cloak-mode",
            "true",
        ),
        (
            "claude/header-defaults/user-agent",
            "claude/header-defaults/user-agent",
            r#""test-agent""#,
        ),
        (
            "claude/header-defaults/timezone",
            "claude/header-defaults/timezone",
            r#""Asia/Shanghai""#,
        ),
        (
            "claude/claude-code/disable-cloaking-model-list",
            "claude/disable-cloaking-model-list",
            "false",
        ),
        ("xai/inject-x-search", "xai/inject-x-search", "true"),
    ] {
        check_historical_path(
            &format!("oauth/providers/{historical}"),
            &format!("upstream/{current}"),
            value,
        )
        .await;
    }
    check_historical_path(
        "oauth/providers/codex/optimize-multi-agent-v2",
        "client/codex/optimize-multi-agent-v2",
        "true",
    )
    .await;
}

/// Checks that a file holding `value` at v8 path `current` reads it at
/// `historical` too.
async fn check_historical_path(historical: &str, current: &str, value: &str) {
    // The value goes in as a flow mapping: `{a: {b: value}}`, braces off.
    let nested = current
        .rsplit('/')
        .fold(value.to_owned(), |inner, key| format!("{{{key}: {inner}}}"));
    let raw = format!("server: {{port: 8317}}\n{}\n", &nested[1..nested.len() - 1]);
    let (dir, api) = over_file(&raw);
    for path in [historical, current] {
        let answer = api.get(&format!("/v8/management/config/{path}")).await;
        assert_v8(&answer, value);
    }
    assert_v8(&api.get("/v8/management/config/server/port").await, "8317");
    let path = format!("/v8/management/config/{historical}");
    assert_unported(&api, Method::PUT, &path).await;
    assert_unchanged(&dir, &raw);
}

/// Ported from upstream's config_v8_compatibility_test.go
/// (TestConfigV8HistoricalProviderSubtrees): the historical provider
/// subtree shows the shared and client settings too.
#[tokio::test]
async fn config_v8_historical_provider_subtrees() {
    let raw = "upstream: {codex: {response-steering: true, stream-bootstrap-buffering: true}, xai: {inject-x-search: true}}\noauth: {providers: {codex: {header-defaults: {user-agent: oauth-agent}}}}\nclient: {codex: {optimize-multi-agent-v2: true, enable-apply-patch: true}}\n";
    let (dir, api) = over_file(raw);
    assert_v8(
        &api.get("/v8/management/config/oauth/providers/codex").await,
        concat!(
            r#"{"header-defaults":{"user-agent":"oauth-agent"},"optimize-multi-agent-v2":true,"#,
            r#""response-steering":true,"stream-bootstrap-buffering":true}"#,
        ),
    );
    assert_v8(
        &api.get("/v8/management/config/upstream/codex").await,
        r#"{"response-steering":true,"stream-bootstrap-buffering":true}"#,
    );
    for method in [Method::PATCH, Method::PUT, Method::DELETE] {
        let path = "/v8/management/config/oauth/providers/codex";
        assert_unported(&api, method, path).await;
    }
    assert_unchanged(&dir, raw);
}

/// Not upstream's: values the typed config doesn't know are written as
/// upstream writes what yaml.v3 decodes, after it writes the migrated file
/// out and reads it back, so a timestamp in a flow mapping and an empty
/// flow value come back strings. One Go's encoder can't write is a 500
/// `encode_failed`, where `c.JSON` leaves upstream's 200 with no body.
/// Recorded from upstream's `GetConfigV8` handler under Go 1.26.4.
#[tokio::test]
async fn config_v8_values_are_written_as_go_writes_them() {
    let raw = "plugins:\n  configs:\n    t:\n      i: 42\n      neg: -7\n      big: 18446744073709551615\n      f: 1.5\n      e: 1e21\n      small: 0.0000001\n      ts: 2001-12-14t21:59:43.10-05:00\n      date: 2002-12-14\n      bin: !!binary aGVsbG8=\n      n: ~\n      b: yes\n      t2: true\n      s: '<&>'\n      hex: 0x1F\n      oct: 0o17\n      seq: [1, two, 3.0]\n      inf: x\n";
    let (_dir, api) = over_file(raw);
    assert_v8(
        &api.get("/v8/management/config/plugins/configs/t").await,
        concat!(
            r#"{"b":"yes","big":18446744073709551615,"bin":"hello","date":"2002-12-14T00:00:00Z","#,
            r#""e":1e+21,"f":1.5,"hex":31,"i":42,"inf":"x","n":null,"neg":-7,"oct":15,"#,
            r#""s":"\u003c\u0026\u003e","seq":[1,"two",3],"small":1e-7,"t2":true,"#,
            r#""ts":"2001-12-14T21:59:43.1-05:00"}"#,
        ),
    );
    assert_v8(
        &api.get("/v8/management/config/plugins/configs/t/i").await,
        "42",
    );
    // Floats halfway between two shortest decimals round to even.
    let (_dir, api) = over_file(
        "plugins:\n  configs:\n    value: 2156163594508435.25\n    w: [-29290947659102.0625, 2.98023223876953125e-8]\n",
    );
    assert_v8(
        &api.get("/v8/management/config/plugins/configs/value").await,
        "2156163594508435.2",
    );
    assert_v8(
        &api.get("/v8/management/config/plugins/configs").await,
        r#"{"value":2156163594508435.2,"w":[-29290947659102.062,2.9802322387695312e-8]}"#,
    );
    let (_dir, api) = over_file(
        "plugins: {configs: {t: {v: 2024-01-02T03:04:05+24:00, w: 2002-12-14, e: , n: ~}}}\n",
    );
    assert_v8(
        &api.get("/v8/management/config/plugins/configs/t").await,
        r#"{"e":"","n":null,"v":"2024-01-02T03:04:05+24:00","w":"2002-12-14T00:00:00Z"}"#,
    );
    for raw in [
        "plugins: {configs: {t: {v: .inf}}}\n",
        "plugins: {configs: {t: {1: a}}}\n",
        "plugins:\n  configs:\n    t:\n      v: 2024-01-02T03:04:05+24:00\n",
    ] {
        let (_dir, api) = over_file(raw);
        for path in ["plugins/configs/t", "plugins"] {
            let answer = api.get(&format!("/v8/management/config/{path}")).await;
            answer.assert(
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error":"encode_failed"}"#,
            );
            assert_eq!(answer.header("cache-control"), Some("no-store"));
        }
    }
}

/// Not upstream's: a file that doesn't make a v8 layout is answered as
/// upstream answers it, as is one that can't be read.
#[tokio::test]
async fn config_v8_bad_files_fail() {
    let (_dir, api) = over_file("");
    for path in ["/v8/management/config", "/v8/management/config.yaml"] {
        api.get(path).await.assert(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"invalid_config","message":"empty config"}"#,
        );
    }
    for raw in ["a: [\n", "plugins: {configs: {a: !!int x}}\n"] {
        let (_dir, api) = over_file(raw);
        let body = api
            .get("/v8/management/config")
            .await
            .expect(StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"], "invalid_config", "{raw:?}");
        assert!(body["message"].as_str().unwrap().starts_with("yaml: "));
    }
    let (_dir, api) = over_file("port: 1\nmystery: {a: 1}\nserver: {host: h, bogus: 2}\n");
    assert_v8(
        &api.get("/v8/management/config").await,
        r#"{"config-version":8,"server":{"host":"h","port":1}}"#,
    );

    let dir = AuthDir::new();
    let api = Api::over(&dir);
    for path in ["/v8/management/config", "/v8/management/config/server"] {
        let answer = api.get(path).await;
        answer.assert(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"read_failed"}"#,
        );
    }
    let answer = Api::new().get("/v8/management/config").await;
    answer.assert(
        StatusCode::INTERNAL_SERVER_ERROR,
        r#"{"error":"read_failed"}"#,
    );
}

/// Not upstream's: the management key reads as a bcrypt hash, as upstream
/// has it once it has hashed the file's plain key, the same each time; a
/// hash in the file reads as it is.
#[tokio::test]
async fn config_v8_management_key_reads_hashed() {
    let raw = "remote-management: {secret-key: test-password}\n";
    let (dir, api) = over_file(raw);
    let path = "/v8/management/config/management/secret-key";
    let first = api.get(path).await;
    let hash: String = serde_json::from_str(&first.body).unwrap();
    assert!(hash.starts_with("$2a$10$"), "{hash}");
    assert!(bcrypt::verify("test-password", &hash).unwrap());
    assert_eq!(api.get(path).await.body, first.body);
    let whole = api.get("/v8/management/config").await;
    assert!(!whole.body.contains("test-password"), "{}", whole.body);
    assert_unchanged(&dir, raw);

    let hashed = "$2a$10$abcdefghijklmnopqrstuuJ2mE5Pfn6E8o2Yk3bJr6Y3d8bV1mY5a";
    let (_dir, api) = over_file(&format!(
        "remote-management: {{secret-key: \"{hashed}\"}}\n"
    ));
    assert_v8(
        &api.get("/v8/management/config").await,
        &format!(r#"{{"config-version":8,"management":{{"secret-key":"{hashed}"}}}}"#),
    );
}

/// Ported from upstream's server_management_v8_test.go
/// (TestManagementV8RoutesShareAccessControl): the reads under both names
/// take the key, and answer with the empty 404 while the API is off.
#[tokio::test]
async fn management_v8_config_routes_share_access_control() {
    for (name, enabled, authorized, want) in [
        ("authorized", true, true, StatusCode::OK),
        ("missing key", true, false, StatusCode::UNAUTHORIZED),
        ("disabled", false, true, StatusCode::NOT_FOUND),
    ] {
        let dir = AuthDir::new();
        std::fs::write(dir.config_path(), "port: 8317\n").unwrap();
        for route in [
            "/v0/management/config",
            "/v8/management/config",
            "/v0/management/config.yaml",
            "/v8/management/config.yaml",
            "/v8/management/config/server/port",
            "/v0/management/debug",
            "/v0/management/routing/strategy",
            "/v0/management/gemini-api-key",
            "/v0/management/oauth-model-alias",
        ] {
            // A fresh API each time, so that the failures don't get the
            // address banned.
            let config = if enabled {
                dir.config()
            } else {
                Config::default()
            };
            let api = Api::over_with(&dir, config, None);
            let request = if authorized {
                keyed(Method::GET, route, "")
            } else {
                request_from(LOCAL, Method::GET, route, "")
            };
            let answer = api.send(request).await;
            assert_eq!(answer.status, want, "{name}: {route}: {}", answer.body);
            if !enabled {
                assert_eq!(answer.body, "", "{name}: {route}");
            }
        }
    }
}

/// Ported from upstream's server_management_v8_test.go
/// (TestManagementV8IndependentContract), its config checks: the nested
/// value reads, and the refused writes leave the file alone.
#[tokio::test]
async fn management_v8_independent_contract_config() {
    let raw = "port: 8317\nrequest-retry: 3\ndebug: false\napi-keys: [client]\nremote-management: {secret-key: test-password}\n";
    let (dir, api) = over_file(raw);
    let path = "/v8/management/config/routing/retry/request-retry";
    assert_v8(&api.get(path).await, "3");
    assert_unported(&api, Method::PATCH, "/v8/management/config").await;
    assert_unported(&api, Method::PUT, path).await;
    assert_unported(&api, Method::PUT, "/v0/management/request-retry").await;
    assert_unchanged(&dir, raw);
}

/// Not upstream's: `config.yaml` is the file as it is, not to be cached or
/// sniffed; without a file it is not found.
#[tokio::test]
async fn config_yaml_is_the_file() {
    let (_dir, api) = over_file(LEGACY);
    let answer = api.get("/v0/management/config.yaml").await;
    answer.assert(StatusCode::OK, LEGACY);
    assert_eq!(
        answer.header("content-type"),
        Some("application/yaml; charset=utf-8")
    );
    assert_eq!(answer.header("cache-control"), Some("no-store"));
    assert_eq!(answer.header("x-content-type-options"), Some("nosniff"));

    let not_found = r#"{"error":"not_found","message":"config file not found"}"#;
    let dir = AuthDir::new();
    let answer = Api::over(&dir).get("/v0/management/config.yaml").await;
    answer.assert(StatusCode::NOT_FOUND, not_found);
    let answer = Api::new().get("/v0/management/config.yaml").await;
    answer.assert(StatusCode::NOT_FOUND, not_found);
    for method in [Method::PUT, Method::PATCH, Method::DELETE] {
        assert_unported(&api, method, "/v0/management/config.yaml").await;
    }
}

/// Not upstream's: the config is written as upstream writes it, less the
/// sections open-ferry doesn't type, and with an empty list as `null`.
#[tokio::test]
async fn config_is_written_as_upstream_writes_it() {
    let answer = with_config(RICH).get("/v0/management/config").await;
    let want = format!(
        concat!(
            r#"{{"client":{{"codex":{{"optimize-multi-agent-v2":true,"enable-apply-patch":false}}}},"#,
            r#""proxy-url":"socks5://127.0.0.1:1080","disable-image-generation":false,"#,
            r#""force-model-prefix":true,"request-log":true,"#,
            r#""api-keys":["k1","k2"],"passthrough-headers":true,"#,
            r#""streaming":{{"keepalive-seconds":15,"bootstrap-retries":2}},"#,
            r#""nonstream-keepalive-interval":5,"trusted-proxies":["10.0.0.0/8"],"#,
            r#""tls":{{"enable":true,"cert":"c.pem","key":"k.pem"}},"debug":true,"#,
            r#""commercial-mode":false,"logging-to-file":true,"logs-max-total-size-mb":0,"#,
            r#""error-logs-max-files":10,"usage-statistics-enabled":false,"#,
            r#""redis-usage-queue-retention-seconds":60,"#,
            r#""disable-cooling":true,"save-cooldown-status":false,"#,
            r#""transient-error-cooldown-seconds":7,"#,
            r#""auth-auto-refresh-workers":3,"request-retry":4,"max-retry-credentials":2,"#,
            r#""max-retry-interval":30,"quota-exceeded":{{"switch-project":true,"#,
            r#""switch-preview-model":true,"antigravity-credits":true}},"#,
            r#""routing":{{"strategy":"RR"}},"ws-auth":true,"gemini-api-key":{gemini},"#,
            r#""interactions-api-key":null,"codex-api-key":{codex},"xai-api-key":null,"#,
            r#""meta-api-key":null,"xai":{{"inject-x-search":false}},"#,
            r#""codex":{{"stream-bootstrap-buffering":true,"#,
            r#""orphan-delegation-compatibility":false,"model-level-cooling":true,"#,
            r#""response-steering":false}},"codex-header-defaults":{{"beta-features":"x"}},"#,
            r#""claude":{{"model-level-cooling":true}},"claude-api-key":{claude},"#,
            r#""openai-compatibility":[{{"name":"Mimo CN","#,
            r#""base-url":"https://token-plan-cn.xiaomimimo.com/v1","#,
            r#""api-key-entries":[{{"api-key":"test-key","proxy-url":"http://q.example"}}],"#,
            r#""models":[{{"name":"mimo-v2.5","alias":""}}],"support-prompt-cache-key":true,"#,
            r#""disable-cooling":true,"request-retry":0}},"#,
            r#"{{"name":"nokeys","base-url":"https://n.example","models":null}}],"#,
            r#""vertex-api-key":{vertex},"oauth-excluded-models":{{"codex":["a","b"]}},"#,
            r#""oauth-model-alias":{{"codex":[{{"name":"gpt-5","alias":"g5","fork":true}}]}},"#,
            r#""oauth-request-scoped-errors":{{"codex":[{{"status":400,"match":["bad"],"#,
            r#""action":"retry"}}]}},"payload":{payload}}}"#,
        ),
        gemini = RICH_GEMINI,
        codex = RICH_CODEX,
        claude = RICH_CLAUDE,
        vertex = RICH_VERTEX,
        payload = NO_PAYLOAD,
    );
    answer.assert(StatusCode::OK, &want);

    let empty = concat!(
        r#"{"client":{"codex":{"optimize-multi-agent-v2":false,"enable-apply-patch":false}},"#,
        r#""proxy-url":"","disable-image-generation":false,"force-model-prefix":false,"#,
        r#""request-log":false,"api-keys":null,"#,
        r#""passthrough-headers":false,"streaming":{},"trusted-proxies":null,"#,
        r#""tls":{"enable":false,"cert":"","key":""},"debug":false,"commercial-mode":false,"#,
        r#""logging-to-file":false,"logs-max-total-size-mb":0,"error-logs-max-files":10,"#,
        r#""usage-statistics-enabled":false,"redis-usage-queue-retention-seconds":60,"#,
        r#""disable-cooling":false,"save-cooldown-status":false,"#,
        r#""transient-error-cooldown-seconds":0,"#,
        r#""auth-auto-refresh-workers":0,"request-retry":0,"max-retry-credentials":0,"#,
        r#""max-retry-interval":0,"quota-exceeded":{"switch-project":false,"#,
        r#""switch-preview-model":false,"antigravity-credits":false},"routing":{},"#,
        r#""ws-auth":true,"gemini-api-key":null,"interactions-api-key":null,"#,
        r#""codex-api-key":null,"xai-api-key":null,"meta-api-key":null,"#,
        r#""xai":{"inject-x-search":false},"#,
        r#""codex":{"stream-bootstrap-buffering":false,"orphan-delegation-compatibility":false,"#,
        r#""model-level-cooling":false,"response-steering":false},"#,
        r#""codex-header-defaults":{"beta-features":""},"claude":{"model-level-cooling":false},"#,
        r#""claude-api-key":null,"openai-compatibility":null,"vertex-api-key":null,"#,
        r#""payload":{"default":null,"default-raw":null,"override":null,"override-raw":null,"#,
        r#""filter":null}}"#,
    );
    for text in ["port: 1\n", "port: 1\napi-keys: []\ntrusted-proxies: []\n"] {
        let answer = with_config(text).get("/v0/management/config").await;
        answer.assert(StatusCode::OK, empty);
    }
}

/// A legacy config with interactions, xAI and Meta keys, some of which the
/// loader drops: a repeated interactions key, an xAI key without a base URL,
/// and Meta keys that are empty or start with `dca:`.
const NEW_KEYS: &str = r#"port: 1
interactions-api-key:
  - api-key: i1
    priority: 1
    weight: 2
    base-url: https://i.example
    models: [{name: gemini-2.5-flash, alias: nf}]
  - api-key: i1
    base-url: https://i.example
    models: [{name: gemini-2.5-flash, alias: nf}]
xai-api-key:
  - api-key: x1
    base-url: https://x.example
    alpha-search: true
    websockets: true
    excluded-models: [grok-2*]
  - api-key: x2
meta-api-key:
  - api-key: m1
    prefix: team
    headers: {X-A: a}
  - api-key: "dca:abc"
  - api-key: ""
    base-url: https://m.example
xai: {inject-x-search: true}
"#;

/// Not upstream's: the interactions, xAI and Meta keys and the xAI settings
/// are written at upstream's positions, as the loader leaves them (an xAI
/// key loses `alpha-search`, a Meta key gets its default base URL).
/// Recorded from upstream's `GetConfig` under Go 1.26.4.
#[tokio::test]
async fn interactions_xai_and_meta_keys_are_written() {
    let answer = with_config(NEW_KEYS).get("/v0/management/config").await;
    assert_eq!(answer.status, StatusCode::OK);
    let part = concat!(
        r#""ws-auth":true,"gemini-api-key":null,"#,
        r#""interactions-api-key":[{"api-key":"i1","priority":1,"weight":2,"#,
        r#""base-url":"https://i.example","models":[{"name":"gemini-2.5-flash","alias":"nf"}]}],"#,
        r#""codex-api-key":null,"#,
        r#""xai-api-key":[{"api-key":"x1","base-url":"https://x.example","websockets":true,"#,
        r#""proxy-url":"","models":null,"excluded-models":["grok-2*"]}],"#,
        r#""meta-api-key":[{"api-key":"m1","prefix":"team","base-url":"https://api.meta.ai/v1","#,
        r#""proxy-url":"","models":null,"headers":{"X-A":"a"}}],"#,
        r#""xai":{"inject-x-search":true},"codex":{"#,
    );
    assert!(answer.body.contains(part), "{}", answer.body);
}

/// Not upstream's: the interactions, xAI and Meta keys read in the v8
/// layout, grouped under `api-keys`, and the xAI settings under
/// `upstream.xai` and its historical `oauth.providers.xai`, from a legacy
/// file and from a v8 one. The legacy file is read as it is, with the
/// entries the loader drops. Recorded from upstream's `ConfigV8` (v8.0.11)
/// under Go 1.26.4.
#[tokio::test]
async fn config_v8_reads_interactions_xai_and_meta_keys() {
    let interactions = concat!(
        r#"[{"base-url":"https://i.example","keys":[{"api-key":"i1","weight":2}],"#,
        r#""models":[{"alias":"nf","name":"gemini-2.5-flash"}],"name":"interactions-1","#,
        r#""priority":1},{"base-url":"https://i.example","keys":[{"api-key":"i1"}],"#,
        r#""models":[{"alias":"nf","name":"gemini-2.5-flash"}],"name":"interactions-2"}]"#,
    );
    let meta = concat!(
        r#"[{"headers":{"X-A":"a"},"keys":[{"api-key":"m1"}],"name":"meta-1","prefix":"team"},"#,
        r#"{"keys":[{"api-key":"dca:abc"}],"name":"meta-2"},"#,
        r#"{"base-url":"https://m.example","keys":[{"api-key":""}],"name":"meta-3"}]"#,
    );
    let xai = concat!(
        r#"[{"base-url":"https://x.example","excluded-models":["grok-2*"],"#,
        r#""keys":[{"alpha-search":true,"api-key":"x1","websockets":true}],"name":"xai-1"},"#,
        r#"{"keys":[{"api-key":"x2"}],"name":"xai-2"}]"#,
    );
    let (dir, api) = over_file(NEW_KEYS);
    let keys = format!(r#"{{"interactions":{interactions},"meta":{meta},"xai":{xai}}}"#);
    let whole = format!(
        r#"{{"api-keys":{keys},"config-version":8,"server":{{"port":1}},"upstream":{{"xai":{{"inject-x-search":true}}}}}}"#
    );
    for (path, body) in [
        ("", whole.as_str()),
        ("api-keys", keys.as_str()),
        ("api-keys/interactions", interactions),
        ("api-keys/xai", xai),
        ("api-keys/meta", meta),
        ("upstream/xai", r#"{"inject-x-search":true}"#),
        ("upstream/xai/inject-x-search", "true"),
        ("oauth/providers/xai", r#"{"inject-x-search":true}"#),
        ("oauth/providers/xai/inject-x-search", "true"),
    ] {
        let answer = api.get(&format!("/v8/management/config/{path}")).await;
        assert_v8(&answer, body);
    }
    for path in ["xai-api-key", "xai/inject-x-search"] {
        let answer = api.get(&format!("/v8/management/config/{path}")).await;
        answer.assert(StatusCode::NOT_FOUND, r#"{"error":"not_found"}"#);
    }
    assert_unported(&api, Method::PUT, "/v8/management/config/api-keys/xai").await;
    assert_unchanged(&dir, NEW_KEYS);

    let raw = concat!(
        "config-version: 8
api-keys:
  interactions:
",
        "    - base-url: https://i.example
      keys: [{api-key: i1, weight: 2}]
",
        "  xai:
    - base-url: https://x.example
      excluded-models: [grok-2*]
",
        "      keys: [{api-key: x1}, {api-key: x2}]
",
        "  meta:
    - prefix: team
      keys: [{api-key: m1}]
",
        "upstream:
  xai: {inject-x-search: true}
",
    );
    let (dir, api) = over_file(raw);
    let interactions = r#"[{"base-url":"https://i.example","keys":[{"api-key":"i1","weight":2}]}]"#;
    let xai = concat!(
        r#"[{"base-url":"https://x.example","excluded-models":["grok-2*"],"#,
        r#""keys":[{"api-key":"x1"},{"api-key":"x2"}]}]"#,
    );
    let meta = r#"[{"keys":[{"api-key":"m1"}],"prefix":"team"}]"#;
    let keys = format!(r#"{{"interactions":{interactions},"meta":{meta},"xai":{xai}}}"#);
    let whole = format!(
        r#"{{"api-keys":{keys},"config-version":8,"upstream":{{"xai":{{"inject-x-search":true}}}}}}"#
    );
    for (path, body) in [
        ("", whole.as_str()),
        ("api-keys/interactions", interactions),
        ("api-keys/xai", xai),
        ("api-keys/meta", meta),
        ("oauth/providers/xai/inject-x-search", "true"),
    ] {
        let answer = api.get(&format!("/v8/management/config/{path}")).await;
        assert_v8(&answer, body);
    }
    assert_unchanged(&dir, raw);
}

/// Not upstream's: `disable-image-generation` is written after `proxy-url`
/// as upstream's `MarshalJSON` writes the mode, from either layout, and
/// the v8 reads give it at `multimedia.disable-image-generation` as the
/// file has it. Recorded from upstream's `GetConfig` and `ConfigV8`
/// (v8.0.11) under Go 1.26.4.
#[tokio::test]
async fn disable_image_generation_is_read() {
    for (raw, written, v8) in [
        ("port: 1\n", "false", None),
        (
            "port: 1\ndisable-image-generation: false\n",
            "false",
            Some("false"),
        ),
        (
            "port: 1\ndisable-image-generation: true\n",
            "true",
            Some("true"),
        ),
        (
            "port: 1\ndisable-image-generation: yes\n",
            "true",
            Some(r#""yes""#),
        ),
        (
            "port: 1\ndisable-image-generation: ' Chat '\n",
            r#""chat""#,
            Some(r#"" Chat ""#),
        ),
        (
            "port: 1\ndisable-image-generation: passthrough\n",
            r#""passthrough""#,
            Some(r#""passthrough""#),
        ),
        (
            "server: {port: 1}\nmultimedia: {disable-image-generation: chat}\n",
            r#""chat""#,
            Some(r#""chat""#),
        ),
    ] {
        let answer = with_config(raw).get("/v0/management/config").await;
        assert_eq!(answer.status, StatusCode::OK);
        let part = format!(
            r#""proxy-url":"","disable-image-generation":{written},"force-model-prefix":false,"#
        );
        assert!(answer.body.contains(&part), "{raw}: {}", answer.body);

        let (dir, api) = over_file(raw);
        let path = "/v8/management/config/multimedia/disable-image-generation";
        match v8 {
            Some(value) => {
                assert_v8(&api.get(path).await, value);
                let multimedia = format!(r#"{{"disable-image-generation":{value}}}"#);
                assert_v8(
                    &api.get("/v8/management/config/multimedia").await,
                    &multimedia,
                );
                let whole = format!(
                    r#"{{"config-version":8,"multimedia":{multimedia},"server":{{"port":1}}}}"#
                );
                assert_v8(&api.get("/v8/management/config").await, &whole);
            }
            None => {
                let answer = api.get(path).await;
                answer.assert(StatusCode::NOT_FOUND, r#"{"error":"not_found"}"#);
            }
        }
        let answer = api
            .get("/v8/management/config/disable-image-generation")
            .await;
        answer.assert(StatusCode::NOT_FOUND, r#"{"error":"not_found"}"#);
        assert_unchanged(&dir, raw);
    }
}

/// The `payload` upstream writes when the file has none.
const NO_PAYLOAD: &str =
    r#"{"default":null,"default-raw":null,"override":null,"override-raw":null,"filter":null}"#;

/// Not upstream's: the observability settings and payload rules are
/// written at upstream's positions, as upstream writes them.
#[tokio::test]
async fn observability_settings_and_payload_are_written() {
    let text = concat!(
        "port: 1\ncommercial-mode: true\nlogs-max-total-size-mb: 5\n",
        "error-logs-max-files: 2\nusage-statistics-enabled: true\n",
        "redis-usage-queue-retention-seconds: 30\nsave-cooldown-status: true\n",
        "payload:\n  override:\n    - models: [{name: gpt-*, headers: {X-A: a*}, ",
        "match: [{a: 1.5}]}]\n      params: {z: [1, ~], a: {b: x}}\n",
        "  filter:\n    - params: [a]\n",
    );
    let answer = with_config(text).get("/v0/management/config").await;
    assert_eq!(answer.status, StatusCode::OK);
    let body = answer.body;
    for part in [
        r#""debug":false,"commercial-mode":true,"logging-to-file":false,"#,
        r#""logs-max-total-size-mb":5,"error-logs-max-files":2,"#,
        r#""usage-statistics-enabled":true,"redis-usage-queue-retention-seconds":30,"#,
        r#""disable-cooling":false,"save-cooldown-status":true,"#,
        concat!(
            r#""payload":{"default":null,"default-raw":null,"override":[{"models":[{"#,
            r#""name":"gpt-*","protocol":"","headers":{"X-A":"a*"},"from-protocol":"","#,
            r#""match":[{"a":1.5}],"not-match":null,"exist":null,"not-exist":null}],"#,
            r#""params":{"a":{"b":"x"},"z":[1,null]}}],"override-raw":null,"#,
            r#""filter":[{"models":null,"params":["a"]}]}}"#,
        ),
    ] {
        assert!(body.contains(part), "{part} in {body}");
    }
}

/// Not upstream's: each setting reads on its own, as upstream writes it.
#[tokio::test]
async fn settings_read_one_at_a_time() {
    let api = with_config(RICH);
    for (path, body) in [
        ("debug", r#"{"debug":true}"#),
        ("logging-to-file", r#"{"logging-to-file":true}"#),
        ("proxy-url", r#"{"proxy-url":"socks5://127.0.0.1:1080"}"#),
        (
            "quota-exceeded/switch-project",
            r#"{"switch-project":true}"#,
        ),
        (
            "quota-exceeded/switch-preview-model",
            r#"{"switch-preview-model":true}"#,
        ),
        ("request-log", r#"{"request-log":true}"#),
        ("ws-auth", r#"{"ws-auth":true}"#),
        ("request-retry", r#"{"request-retry":4}"#),
        ("max-retry-credentials", r#"{"max-retry-credentials":2}"#),
        ("max-retry-interval", r#"{"max-retry-interval":30}"#),
        ("force-model-prefix", r#"{"force-model-prefix":true}"#),
        ("routing/strategy", r#"{"strategy":"round-robin"}"#),
    ] {
        let answer = api.get(&format!("/v0/management/{path}")).await;
        answer.assert(StatusCode::OK, body);
        for method in [Method::PUT, Method::PATCH, Method::DELETE] {
            assert_unported(&api, method, &format!("/v0/management/{path}")).await;
        }
    }
    let api = with_config("port: 1\n");
    for (path, body) in [
        ("proxy-url", r#"{"proxy-url":""}"#),
        ("request-retry", r#"{"request-retry":0}"#),
        ("routing/strategy", r#"{"strategy":"round-robin"}"#),
    ] {
        let answer = api.get(&format!("/v0/management/{path}")).await;
        answer.assert(StatusCode::OK, body);
    }
}

/// Not upstream's: the lists read as upstream writes them, with `[]` for
/// an empty key list and `null` for empty OAuth settings and API keys.
#[tokio::test]
async fn lists_are_written_as_upstream_writes_them() {
    let api = with_config(RICH);
    let compat = concat!(
        r#"[{"name":"Mimo CN","disabled":false,"base-url":"https://token-plan-cn.xiaomimimo.com/v1","#,
        r#""api-key-entries":[{"api-key":"test-key","proxy-url":"http://q.example"}],"#,
        r#""models":[{"name":"mimo-v2.5","alias":""}],"support-prompt-cache-key":true,"#,
        r#""disable-cooling":true,"request-retry":0},"#,
        r#"{"name":"nokeys","disabled":false,"base-url":"https://n.example"}]"#,
    );
    for (name, list) in [
        ("api-keys", r#"["k1","k2"]"#),
        ("gemini-api-key", RICH_GEMINI),
        ("claude-api-key", RICH_CLAUDE),
        ("codex-api-key", RICH_CODEX),
        ("vertex-api-key", RICH_VERTEX),
        ("openai-compatibility", compat),
        ("oauth-excluded-models", r#"{"codex":["a","b"]}"#),
        (
            "oauth-model-alias",
            r#"{"codex":[{"name":"gpt-5","alias":"g5","fork":true}]}"#,
        ),
        (
            "oauth-request-scoped-errors",
            r#"{"codex":[{"status":400,"match":["bad"],"action":"retry"}]}"#,
        ),
    ] {
        let answer = api.get(&format!("/v0/management/{name}")).await;
        answer.assert(StatusCode::OK, &format!(r#"{{"{name}":{list}}}"#));
        for method in [Method::PUT, Method::PATCH, Method::DELETE] {
            assert_unported(&api, method, &format!("/v0/management/{name}")).await;
        }
    }
    let api = with_config("port: 1\ngemini-api-key: []\n");
    for (name, list) in [
        ("api-keys", "null"),
        ("gemini-api-key", "[]"),
        ("claude-api-key", "[]"),
        ("codex-api-key", "[]"),
        ("vertex-api-key", "[]"),
        ("openai-compatibility", "[]"),
        ("oauth-excluded-models", "null"),
        ("oauth-model-alias", "null"),
        ("oauth-request-scoped-errors", "null"),
    ] {
        let answer = api.get(&format!("/v0/management/{name}")).await;
        answer.assert(StatusCode::OK, &format!(r#"{{"{name}":{list}}}"#));
    }
}

/// Ported from upstream's config_openai_compat_test.go
/// (TestGetOpenAICompatIncludesDisableCooling): a provider's
/// `support-prompt-cache-key`, `disable-cooling` and `request-retry` are
/// listed, a zero retry count too.
#[tokio::test]
async fn openai_compatibility_includes_disable_cooling() {
    let api = with_config(
        "openai-compatibility:\n  - name: Mimo CN\n    base-url: https://token-plan-cn.xiaomimimo.com/v1\n    api-key-entries: [{api-key: test-key}]\n    models: [{name: mimo-v2.5, alias: ''}]\n    support-prompt-cache-key: true\n    disable-cooling: true\n    request-retry: 0\n",
    );
    let body = api
        .get("/v0/management/openai-compatibility")
        .await
        .expect(StatusCode::OK);
    let entries = body["openai-compatibility"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{body}");
    assert_eq!(entries[0]["support-prompt-cache-key"], true);
    assert_eq!(entries[0]["disable-cooling"], true);
    assert_eq!(entries[0]["request-retry"], 0);
}

/// Not upstream's: each config entry shows the `auth-index` of the
/// credential it made, when the manager holds that credential: an
/// OpenAI-compatible provider's on each key, or on the provider when it has
/// none.
#[tokio::test]
async fn provider_keys_show_their_credentials_auth_index() {
    let text = "gemini-api-key:\n  - {api-key: g1, headers: {X-B: b, X-A: a}}\n  - {api-key: g2}\nclaude-api-key: [{api-key: a1, base-url: https://a.example}]\ncodex-api-key: [{api-key: c1, base-url: https://c.example, prefix: team}]\nopenai-compatibility:\n  - {name: ' Mimo ', base-url: https://m.example, api-key-entries: [{api-key: k1}, {api-key: k2}]}\n  - {name: nokeys, base-url: ' https://n.example '}\nvertex-api-key: [{api-key: v1, base-url: https://v.example}]\n";
    let config = Config::parse(text).unwrap();
    let ctx = SynthesisContext::new(AuthDir::new().path(), chrono::Utc::now());
    let auths = synthesize_config_auths(&config, &ctx, &mut StableIdGenerator::new()).unwrap();
    let api = with_config(text);
    // In order: Gemini g1 and g2, Claude, Codex, the two keys of Mimo,
    // nokeys and Vertex. The manager doesn't hold g2's credential.
    let indexes: Vec<Option<String>> = auths
        .into_iter()
        .enumerate()
        .map(|(at, auth)| (at != 1).then(|| api.register(auth)))
        .collect();
    assert_eq!(indexes.len(), 8);
    let list = async |name: &str| -> Vec<Value> {
        let body = api
            .get(&format!("/v0/management/{name}"))
            .await
            .expect(StatusCode::OK);
        body[name].as_array().unwrap().clone()
    };
    let index = |entry: &Value| entry["auth-index"].as_str().map(str::to_owned);

    let gemini = list("gemini-api-key").await;
    let shown: Vec<Option<String>> = gemini.iter().map(index).collect();
    assert_eq!(shown, [indexes[0].clone(), None]);
    for (name, at) in [
        ("claude-api-key", 2),
        ("codex-api-key", 3),
        ("vertex-api-key", 7),
    ] {
        let entries = list(name).await;
        assert_eq!(index(&entries[0]), indexes[at], "{name}");
    }
    let compat = list("openai-compatibility").await;
    let keys = compat[0]["api-key-entries"].as_array().unwrap();
    assert_eq!(index(&keys[0]), indexes[4]);
    assert_eq!(index(&keys[1]), indexes[5]);
    assert_eq!(index(&compat[0]), None);
    assert_eq!(compat[1]["base-url"], "https://n.example");
    assert_eq!(index(&compat[1]), indexes[6]);
}
