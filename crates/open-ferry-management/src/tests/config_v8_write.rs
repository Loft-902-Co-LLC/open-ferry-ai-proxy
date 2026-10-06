// Ported from CLIProxyAPI internal/api/handlers/management/
// config_v8_test.go (TestConfigV8MigrationAndLegacyAPI,
// TestConfigV8CommentsUnknownLegacySectionsOnWrite,
// TestConfigV8CommentsUnknownNestedFieldsOnWrite,
// TestV8NestedWriteMigratesOnlyOnSuccess,
// TestV8MigrationReloadSnapshotMatchesDisk,
// TestV8GroupedCredentialsSurviveLegacyWrites, TestConfigV8DeleteLastField,
// TestConfigV8ReplaceEmptyGroup, TestConfigV8EmptyExcludedModelsSurvivesSave,
// TestConfigV8JSONTURNSecrets, TestConfigV8DeletePreservesDocumentPresence),
// config_v8_compatibility_test.go (TestConfigV8HistoricalFieldPaths,
// TestConfigV8HistoricalProviderSubtrees,
// TestConfigV8HistoricalConfigurationBodies,
// TestLegacyConfigYAMLSavesBySubmittedVersion,
// TestV0SetterSavesByExistingVersion, TestConfigV8HistoricalNullContainerPatch,
// TestV0RepeatedSavesKeepLegacyClientAlias,
// TestConfigV8FieldUpdatesKeepComments), config_v8_client_test.go
// (TestConfigV8ClientMultiAgentMigration), config_v8_upstream_test.go
// (TestConfigV8SharedUpstreamRoundTrip), config_v8_auth_index_test.go
// (TestConfigV8APIKeysExposeAuthIndex_Issue6287, cases 5, 6 and 10),
// config_priority_test.go and config_claude_key_test.go
// (TestPatchClaudeKeyPriority) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The routes that write the config, over a config file in a temporary
//! directory: saved by the [`FileConfigWriter`](crate::FileConfigWriter)
//! the service uses and loaded again after each save, as the service does
//! (see [`Api::over_config_file`]).
//!
//! Upstream's tests check the file written, the config the handlers read
//! and the snapshot its reload hook gets. Here each write's file is checked
//! the same way, and the config the handlers read is checked to be the one
//! the file loads as after each save, which covers upstream's reload
//! snapshot checks.
//!
//! The expected files are upstream's, recorded from its `ConfigV8`,
//! `PutConfigYAML` and `PutClaudeKeys` (v8.0.15) under Go 1.27.1 for the
//! same requests.
//!
//! Deviations from upstream:
//! - The settings this port doesn't type (cloaking, header defaults, the
//!   live media relay, plugins) are checked in the file, or through the v8
//!   reads, rather than in the loaded config. Upstream's `ForAPIKey` is
//!   checked as the legacy names the loaded config keeps OAuth-only.
//! - `TestConfigV8MigrationAndLegacyAPI` checks that the write keeps the
//!   file's inode (`os.SameFile`); writes here replace the file atomically,
//!   so that check is dropped.
//! - `TestConfigV8CommentsUnknownLegacySectionsOnWrite` and
//!   `TestConfigV8DeletePreservesDocumentPresence` give the handler a Home
//!   config at run time and check that writes keep it; Home isn't ported,
//!   so those checks are dropped.
//! - `TestConfigV8EmptyExcludedModelsSurvivesSave` checks that loading a
//!   file with a conflicting legacy `oauth-excluded-models` removes it from
//!   the file. Loading never writes the file here, so only the rules that
//!   take effect are checked.
//! - A value of the wrong type is refused with a message that doesn't quote
//!   it (see `open_ferry_core::config`); upstream's quotes it. The tests
//!   check the status, as upstream's do.

use http::{Method, StatusCode};
use open_ferry_core::config::v8_edit::validate_v8_config;
use open_ferry_core::config::{AnyValue, Config, V8Document};
use serde_json::{Value, json};

use super::{Api, AuthDir};

/// The whole v8 config.
const CONFIG: &str = "/v8/management/config";

/// What a v8 write answers when it succeeds.
const V8_OK: &str = r#"{"config-version":8,"status":"ok"}"#;

/// What a v0 write answers when it succeeds.
const OK: &str = r#"{"status":"ok"}"#;

/// The API over a temporary config file holding `raw`.
fn over(raw: &str) -> (AuthDir, Api) {
    let dir = AuthDir::new();
    let api = Api::over_config_file(&dir, raw);
    (dir, api)
}

/// The v8 route of `path`.
fn at(path: &str) -> String {
    format!("{CONFIG}/{path}")
}

/// `method path` with `body`, after checking that it answers `status`: the
/// body answered.
async fn request(api: &Api, method: Method, path: &str, body: &str, status: StatusCode) -> String {
    let answer = api.call(method.clone(), path, body).await;
    assert_eq!(
        answer.status, status,
        "{method} {path} {body}: {}",
        answer.body
    );
    answer.body
}

/// The config file's contents.
fn read(dir: &AuthDir) -> String {
    std::fs::read_to_string(dir.config_path()).unwrap()
}

/// The config the file loads as, after checking that it is the one the
/// handlers read.
fn loaded(api: &Api, dir: &AuthDir) -> Config {
    let config = Config::load(dir.config_path()).unwrap();
    assert!(
        *api.state.config() == config,
        "the config the handlers read isn't the one the file loads as"
    );
    config
}

/// The file's value at the v8 `path`, keys split on `/`, or `None` when
/// there is none.
fn value_at(dir: &AuthDir, path: &str) -> Option<AnyValue> {
    let document = V8Document::migrate(read(dir).as_bytes()).unwrap();
    let parts: Vec<&str> = path.split('/').collect();
    document.value(&parts).map(Result::unwrap)
}

/// Checks that the file is a valid config in the v8 layout, and returns it.
fn assert_v8_file(dir: &AuthDir) -> String {
    let data = read(dir);
    if let Err(error) = validate_v8_config(data.as_bytes()) {
        panic!("{error}\n{data}");
    }
    assert!(data.contains("config-version: 8"), "{data}");
    data
}

/// Whether the legacy setting `name`, `on` in the config, is on for
/// API-key credentials too: upstream's `ForAPIKey` drops the settings a v8
/// file sets for OAuth only.
fn for_api_key(config: &Config, name: &str, on: bool) -> bool {
    on && !config.oauth_only_fields().contains(name)
}

/// The legacy config of upstream's `TestConfigV8MigrationAndLegacyAPI`.
const LEGACY: &str = "# Keep this configuration\nport: 8317\nrequest-retry: 3\napi-keys: [client]\nws-auth: true\ntls: {}\npayload: null\ncodex: {live-media-relay: {}}\n";

/// [`LEGACY`] after a v0 write, then a v8 `PATCH`, as upstream writes it.
const LEGACY_PATCHED: &str = "access:
    api-keys:
        - client
server:
# Keep this configuration
    port: 8317
    discovery:
        service-type: _ai-gateway._tcp
        subtypes:
            - _chat-completions
            - _responses
            - _messages
            - _generate-content
            - _interactions
    tls: {}
credentials:
    concurrency:
        cpa-heartbeat-timeout: 3s
        cpa-cancel-bound: 5s
        reclaim-grace: 5s
        cleanup-interval: 5s
        release-flush-interval: 250ms
        release-max-backoff: 2s
        busy-retry-min: 250ms
        busy-retry-max: 1s
        max-limit: 1000000
    in-flight:
        snapshot-interval: 2s
        stale-after: 10s
        max-part-bytes: 262144
        max-part-count: 64
        max-revision-bytes: 16777216
        max-aggregate-groups: 100000
        max-details: 10000
        max-string-bytes: 256
        staging-retention: 1m
observability:
    usage:
        redis-usage-queue-retention-seconds: 60
routing:
    cooldown:
        disable-cooling: false
    retry:
        request-retry: 0
oauth:
    providers:
        aistudio:
            ws-auth: false
        codex:
            live-media-relay: {}
requests:
    payload:
        default: []
        default-raw: []
        override: []
        override-raw: []
        filter: []
config-version: 8
";

/// Ported from upstream's config_v8_test.go
/// (TestConfigV8MigrationAndLegacyAPI): reads and refused writes leave a
/// legacy file as it is, a v0 write keeps its layout, a v8 write moves it
/// into the v8 layout with its comments, and later writes keep that.
#[tokio::test]
async fn config_v8_migration_and_legacy_api() {
    let (dir, api) = over(LEGACY);
    request(&api, Method::GET, CONFIG, "", StatusCode::OK).await;
    assert_eq!(read(&dir), LEGACY, "GET migrated the config");
    let invalid = r#"{"server":{"port":"invalid"}}"#;
    request(
        &api,
        Method::PATCH,
        CONFIG,
        invalid,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert_eq!(read(&dir), LEGACY, "failed write migrated the config");

    let retry = "/v0/management/request-retry";
    let answer = request(&api, Method::PUT, retry, r#"{"value":2}"#, StatusCode::OK).await;
    assert_eq!(answer, OK);
    let saved = read(&dir);
    assert!(
        !saved.contains("config-version") && !saved.contains("routing:"),
        "v0 migrated a legacy file: {saved}"
    );

    let patch = r#"{"routing":{"retry":{"request-retry":0}},"oauth":{"providers":{"aistudio":{"ws-auth":false}}}}"#;
    let answer = request(&api, Method::PATCH, CONFIG, patch, StatusCode::OK).await;
    assert_eq!(answer, V8_OK);
    assert_eq!(assert_v8_file(&dir), LEGACY_PATCHED);
    let config = loaded(&api, &dir);
    assert_eq!(
        (config.request_retry, config.ws_auth, config.api_keys.len()),
        (0, false, 1),
        "v8 patch lost effective values"
    );

    request(&api, Method::PUT, retry, r#"{"value":5}"#, StatusCode::OK).await;
    assert_eq!(loaded(&api, &dir).request_retry, 5);
    assert_v8_file(&dir);

    let proxy = at("requests/proxy-url");
    request(&api, Method::PUT, &proxy, r#""direct""#, StatusCode::OK).await;
    assert_eq!(loaded(&api, &dir).proxy_url, "direct");
    let saved = read(&dir);
    for (path, body) in [
        ("server/unknown-option", "true"),
        ("credentials/concurrency/lifecycle-config-revision", "999"),
    ] {
        request(&api, Method::PUT, &at(path), body, StatusCode::BAD_REQUEST).await;
    }
    assert_eq!(read(&dir), saved);
}

/// Ported from upstream's config_v8_test.go
/// (TestConfigV8CommentsUnknownLegacySectionsOnWrite): sections the v8
/// layout doesn't know are kept as comments by the first write, once, and
/// a write that names one is refused.
#[tokio::test]
async fn config_v8_comments_unknown_legacy_sections_on_write() {
    let raw = "home: {enabled: true, host: ignored.example}\nenable-gemini-cli-endpoint: false\nformer-feature: {mode: old}\nserver: {port: 8317}\nproxy-url: \"\"\n";
    let (dir, api) = over(raw);
    request(&api, Method::GET, CONFIG, "", StatusCode::OK).await;
    for (body, status) in [
        (
            r#"{"server":{"port":"invalid"}}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (r#"{"home":{"enabled":true}}"#, StatusCode::BAD_REQUEST),
        (r#"{"unknown-setting":true}"#, StatusCode::BAD_REQUEST),
    ] {
        request(&api, Method::PATCH, CONFIG, body, status).await;
    }
    assert_eq!(read(&dir), raw, "read or failed write changed the config");

    let proxy = at("requests/proxy-url");
    request(&api, Method::PUT, &proxy, r#""direct""#, StatusCode::OK).await;
    let saved = assert_v8_file(&dir);
    assert_eq!(
        saved,
        "server: {port: 8317}\nrequests:\n    proxy-url: \"direct\"\nconfig-version: 8\n\n# home: {enabled: true, host: ignored.example}\n# enable-gemini-cli-endpoint: false\n# former-feature: {mode: old}\n"
    );
    for key in ["home", "enable-gemini-cli-endpoint", "former-feature"] {
        assert!(value_at(&dir, key).is_none(), "{key} is still a section");
    }
    assert_eq!(
        value_at(&dir, "requests/proxy-url"),
        Some(AnyValue::Str("direct".into()))
    );
    assert_eq!(loaded(&api, &dir).proxy_url, "direct");

    request(&api, Method::PUT, &proxy, r#""none""#, StatusCode::OK).await;
    request(&api, Method::DELETE, &at("server"), "", StatusCode::OK).await;
    let saved = read(&dir);
    for key in ["home", "enable-gemini-cli-endpoint", "former-feature"] {
        let comment = format!("# {key}:");
        assert_eq!(saved.matches(&comment).count(), 1, "{key}: {saved}");
    }
    loaded(&api, &dir);
}

/// Ported from upstream's config_v8_test.go
/// (TestConfigV8CommentsUnknownNestedFieldsOnWrite): an unknown field in a
/// known section is kept as a comment by the first write, once, and a
/// write that adds one is refused.
#[tokio::test]
async fn config_v8_comments_unknown_nested_fields_on_write() {
    let raw = "server: {port: 8317}\noauth: {providers: {codex: {disable-codex-cloaking: true, retired-setting: false}}}\n";
    let (dir, api) = over(raw);
    request(&api, Method::GET, CONFIG, "", StatusCode::OK).await;
    assert_eq!(read(&dir), raw, "GET changed the config");
    let patch = r#"{"server":{"port":8318}}"#;
    request(&api, Method::PATCH, CONFIG, patch, StatusCode::OK).await;
    let saved = assert_v8_file(&dir);
    assert_eq!(
        saved,
        "server: {port: 8318}\noauth: {providers: {codex: {}}}\nupstream:\n    codex:\n        disable-codex-cloaking: true\nconfig-version: 8\n\n# oauth.providers.codex.retired-setting: false\n"
    );
    assert_eq!(loaded(&api, &dir).port, 8318);
    let cloaking = at("upstream/codex/disable-codex-cloaking");
    assert_eq!(
        request(&api, Method::GET, &cloaking, "", StatusCode::OK).await,
        "true"
    );

    let added = at("oauth/providers/codex/new-setting");
    request(&api, Method::PUT, &added, "true", StatusCode::BAD_REQUEST).await;
    assert_eq!(read(&dir), saved, "invalid new setting changed the config");
    request(&api, Method::DELETE, &cloaking, "", StatusCode::OK).await;
    let saved = read(&dir);
    let comment = "# oauth.providers.codex.retired-setting: false";
    assert_eq!(saved.matches(comment).count(), 1, "{saved}");
    loaded(&api, &dir);
}

/// Ported from upstream's config_v8_test.go
/// (TestV8NestedWriteMigratesOnlyOnSuccess): a legacy file is moved into
/// the v8 layout only by a write that succeeds.
#[tokio::test]
async fn v8_nested_write_migrates_only_on_success() {
    for (body, status, migrated) in [
        ("0", StatusCode::OK, true),
        (r#""bad""#, StatusCode::UNPROCESSABLE_ENTITY, false),
        (r#"{"value":0}"#, StatusCode::UNPROCESSABLE_ENTITY, false),
    ] {
        let raw = "request-retry: 3\n";
        let (dir, api) = over(raw);
        let path = at("routing/retry/request-retry");
        request(&api, Method::PUT, &path, body, status).await;
        let data = read(&dir);
        assert_eq!(
            data.contains("config-version: 8"),
            migrated,
            "{body}: {data}"
        );
        if !migrated {
            assert_eq!(data, raw);
        }
        loaded(&api, &dir);
    }
}

/// Ported from upstream's config_v8_test.go
/// (TestV8MigrationReloadSnapshotMatchesDisk): after a v0 or v8 write, the
/// config the handlers read is the one the file loads as, and the shared
/// provider settings still apply to API-key credentials.
#[tokio::test]
async fn v8_migration_reload_snapshot_matches_disk() {
    for (path, body, migrated) in [
        ("/v0/management/debug", r#"{"value":true}"#, false),
        (
            "/v8/management/config/observability/logs/debug",
            "true",
            true,
        ),
        (
            "/v8/management/config/plugins/configs/test-plugin/enabled",
            "false",
            true,
        ),
        (CONFIG, r#"{"observability":{"logs":{"debug":true}}}"#, true),
    ] {
        let raw = "codex: {disable-codex-cloaking: true}\nxai: {inject-x-search: true}\n";
        let (dir, api) = over(raw);
        request(&api, Method::PATCH, path, body, StatusCode::OK).await;
        let config = loaded(&api, &dir);
        let inject = config.xai.inject_x_search;
        assert!(
            for_api_key(&config, "xai.inject-x-search", inject),
            "{path}"
        );
        let cloaking = request(
            &api,
            Method::GET,
            &at("upstream/codex/disable-codex-cloaking"),
            "",
            StatusCode::OK,
        )
        .await;
        assert_eq!(cloaking, "true", "{path}");
        if migrated {
            assert_v8_file(&dir);
        }
    }
}

/// Ported from upstream's config_v8_test.go
/// (TestV8GroupedCredentialsSurviveLegacyWrites): a v0 write keeps a v8
/// key group, and a v0 key list replaces it.
#[tokio::test]
async fn v8_grouped_credentials_survive_legacy_writes() {
    let raw = "api-keys:
  codex:
    - name: production
      base-url: https://example.invalid
      headers: {X-Shared: value}
      request-retry: 2
      keys:
        - api-key: first
          request-retry: null
        - api-key: second
          request-retry: 0
";
    let (dir, api) = over(raw);
    request(
        &api,
        Method::PUT,
        "/v0/management/debug",
        r#"{"value":true}"#,
        StatusCode::OK,
    )
    .await;
    assert!(
        read(&dir).contains("production"),
        "unrelated legacy write lost group identity"
    );
    let keys = r#"[{"api-key":"first","base-url":"https://example.invalid","request-retry":0}]"#;
    request(
        &api,
        Method::PUT,
        "/v0/management/codex-api-key",
        keys,
        StatusCode::OK,
    )
    .await;
    let config = loaded(&api, &dir);
    assert_eq!(config.codex_api_key.len(), 1);
    assert_eq!(config.codex_api_key[0].request_retry, Some(0));
}

/// What a test checks in the config a deletion leaves.
type Check = fn(&Config) -> bool;

/// Ported from upstream's config_v8_test.go (TestConfigV8DeleteLastField):
/// deleting the last field of a section restores its default, leaves the
/// rest as it was, and the field is gone.
#[tokio::test]
async fn config_v8_delete_last_field() {
    let cases: [(&str, &str, Check); 6] = [
        ("request-retry: 3\n", "routing/retry/request-retry", |c| {
            c.request_retry == 0
        }),
        (
            "ws-auth: false\n",
            "oauth/providers/aistudio/ws-auth",
            |c| c.ws_auth,
        ),
        ("debug: true\n", "observability/logs/debug", |c| !c.debug),
        (
            "routing: {strategy: fill-first, retry: {request-retry: 3}}\n",
            "routing/retry/request-retry",
            |c| c.request_retry == 0 && c.routing.strategy == "fill-first",
        ),
        // The cloaking flag isn't typed: its absence is checked below.
        (
            "oauth: {providers: {codex: {disable-codex-cloaking: true}}}\n",
            "upstream/codex/disable-codex-cloaking",
            |_| true,
        ),
        (
            "oauth: {excluded-models: {codex: [blocked-model]}}\n",
            "oauth/excluded-models",
            |c| c.oauth_excluded_models.is_empty(),
        ),
    ];
    for (raw, path, check) in cases {
        let raw = format!(
            "{raw}port: 8317\napi-keys: [client]\nplugins: {{configs: {{sample: {{enabled: false, options: {{}}}}}}}}\n"
        );
        let (dir, api) = over(&raw);
        let url = at(path);
        request(&api, Method::DELETE, &url, "", StatusCode::OK).await;
        let config = loaded(&api, &dir);
        assert!(
            check(&config) && config.port == 8317 && config.api_keys == ["client"],
            "{path}: delete did not restore defaults or changed unrelated settings"
        );
        assert!(
            value_at(&dir, path).is_none(),
            "{path}: save reintroduced the field"
        );
        assert_eq!(
            value_at(&dir, "plugins/configs/sample/options"),
            Some(AnyValue::Map(Default::default())),
            "{path}: delete removed an unrelated empty mapping"
        );
        for method in [Method::GET, Method::DELETE] {
            request(&api, method, &url, "", StatusCode::NOT_FOUND).await;
        }
    }
}

/// Ported from upstream's config_v8_test.go (TestConfigV8ReplaceEmptyGroup):
/// replacing a group with an empty one restores its defaults.
#[tokio::test]
async fn config_v8_replace_empty_group() {
    for (path, raw) in [
        ("routing/retry", "routing: {retry: {request-retry: 3}}\n"),
        (
            "oauth/providers/aistudio",
            "oauth: {providers: {aistudio: {ws-auth: false}}}\n",
        ),
        (
            "observability/logs",
            "observability: {logs: {debug: true}}\n",
        ),
    ] {
        let (dir, api) = over(raw);
        request(&api, Method::PUT, &at(path), "{}", StatusCode::OK).await;
        let config = loaded(&api, &dir);
        assert!(
            config.request_retry == 0 && config.ws_auth && !config.debug,
            "{path}: empty replacement did not restore defaults"
        );
    }
}

/// Ported from upstream's config_v8_test.go
/// (TestConfigV8EmptyExcludedModelsSurvivesSave): an explicitly empty
/// `oauth.excluded-models` stays in the file through writes, and keeps
/// shadowing legacy rules added to the file later.
#[tokio::test]
async fn config_v8_empty_excluded_models_survives_save() {
    for (rules, path, body) in [
        ("{}", "/v0/management/debug", r#"{"value":true}"#),
        (
            "{}",
            "/v8/management/config/observability/logs/debug",
            "true",
        ),
        (
            "{codex: [blocked-model]}",
            "/v8/management/config/oauth/excluded-models",
            "{}",
        ),
        (
            "{codex: [blocked-model]}",
            "/v0/management/oauth-excluded-models",
            "{}",
        ),
    ] {
        let raw = format!("port: 8317\noauth:\n  excluded-models: {rules}\n");
        let (dir, api) = over(&raw);
        request(&api, Method::PUT, path, body, StatusCode::OK).await;
        let models = at("oauth/excluded-models");
        let answer = request(&api, Method::GET, &models, "", StatusCode::OK).await;
        assert_eq!(
            answer.trim(),
            "{}",
            "{path}: explicit empty setting not kept"
        );
        loaded(&api, &dir);

        // Later manual legacy edits stay shadowed by the explicit v8 empty
        // map.
        let mut saved = read(&dir);
        saved.push_str("\noauth-excluded-models: {codex: [legacy-blocked-model]}\n");
        std::fs::write(dir.config_path(), saved).unwrap();
        let after = Config::load(dir.config_path()).unwrap();
        assert!(
            after.oauth_excluded_models.is_empty(),
            "{path}: legacy rules became effective after saving"
        );
    }
}

/// The TURN servers of upstream's `TestConfigV8JSONTURNSecrets`.
const TURN: &str = "oauth:
  providers:
    codex:
      live-media-relay:
        ice-servers:
          - {urls: ['turn:example.invalid:3478'], username: test-relay-user, credential: test-relay-password}
          - {urls: ['turn:example.invalid:3478'], username: test-relay-user-2, credential: test-relay-password-2}
";

/// Where [`TURN`]'s servers are.
const ICE_SERVERS: &str = "oauth/providers/codex/live-media-relay/ice-servers";

/// The `username` and `credential` of each TURN server in the file, empty
/// where there is none or it is null.
fn turn_users(dir: &AuthDir) -> Vec<(String, String)> {
    let Some(AnyValue::Seq(servers)) = value_at(dir, ICE_SERVERS) else {
        panic!("no TURN servers: {}", read(dir));
    };
    servers
        .iter()
        .map(|server| {
            let AnyValue::Map(fields) = server else {
                panic!("a TURN server isn't a mapping");
            };
            let text = |name: &str| match fields.get(name) {
                Some(AnyValue::Str(text)) => text.clone(),
                _ => String::new(),
            };
            (text("username"), text("credential"))
        })
        .collect()
}

/// Ported from upstream's config_v8_test.go (TestConfigV8JSONTURNSecrets):
/// a JSON read hides the TURN servers' secrets and a JSON write of what it
/// read keeps them; a server with other `urls`, or whose secrets the write
/// clears, has none.
#[tokio::test]
async fn config_v8_json_turn_secrets() {
    let (dir, api) = over(TURN);
    for url in [CONFIG.to_owned(), at(ICE_SERVERS)] {
        let body = request(&api, Method::GET, &url, "", StatusCode::OK).await;
        assert!(
            !body.contains("test-relay-user") && !body.contains("test-relay-password"),
            "JSON exposed TURN credentials"
        );
        request(&api, Method::PUT, &url, &body, StatusCode::OK).await;
        assert_eq!(
            turn_users(&dir),
            [
                ("test-relay-user".into(), "test-relay-password".into()),
                ("test-relay-user-2".into(), "test-relay-password-2".into()),
            ],
            "{url}: JSON round trip changed redacted credentials"
        );
        loaded(&api, &dir);
    }
    let yaml = request(
        &api,
        Method::GET,
        "/v8/management/config.yaml",
        "",
        StatusCode::OK,
    )
    .await;
    assert!(
        yaml.contains("test-relay-password"),
        "YAML export lost TURN credentials"
    );

    for body in [
        r#"[{"urls":["turn:replacement.invalid:3478"]}]"#,
        r#"[{"urls":["turn:example.invalid:3478"],"username":"","credential":null}]"#,
    ] {
        std::fs::write(dir.config_path(), TURN).unwrap();
        request(&api, Method::PUT, &at(ICE_SERVERS), body, StatusCode::OK).await;
        assert_eq!(
            turn_users(&dir),
            [(String::new(), String::new())],
            "{body}: unexpected inherited TURN credentials"
        );
        loaded(&api, &dir);
    }
}

/// The document of upstream's `TestConfigV8DeletePreservesDocumentPresence`.
const PRESENCE: &str = "# Keep document comment
config-version: 8
server: {port: 8317}
routing:
  retry:
    request-retry: 3
    max-retry-interval: 30
plugins:
  configs:
    sample:
      enabled: false
      options: {} # Keep empty mapping
      custom-null: null # Keep explicit null
      custom-tree: {unknown: [one, {two: 2}]}
";

/// Ported from upstream's config_v8_test.go
/// (TestConfigV8DeletePreservesDocumentPresence): a deletion removes only
/// the field named, works on the file as it is now rather than as last
/// loaded, and keeps every other field's presence, value and comments,
/// including empty mappings and explicit nulls, which can be deleted in
/// turn.
#[tokio::test]
async fn config_v8_delete_preserves_document_presence() {
    let (dir, api) = over(PRESENCE);
    let original = V8Document::migrate(PRESENCE.as_bytes()).unwrap();
    let delete = |path: &'static str| at(path);
    request(
        &api,
        Method::DELETE,
        &delete("routing/retry/request-retry"),
        "",
        StatusCode::OK,
    )
    .await;
    loaded(&api, &dir);
    // A sibling written to the file after the first deletion: the next
    // one must start from the file, not from the config last loaded.
    let data = read(&dir).replace(
        "max-retry-interval: 30",
        "max-retry-interval: 30\n        max-retry-credentials: 7 # Keep new sibling",
    );
    std::fs::write(dir.config_path(), data).unwrap();
    request(
        &api,
        Method::DELETE,
        &delete("routing/retry/max-retry-interval"),
        "",
        StatusCode::OK,
    )
    .await;
    loaded(&api, &dir);

    for path in [
        "routing/retry/request-retry",
        "routing/retry/max-retry-interval",
        "observability",
        "server/host",
    ] {
        assert!(
            value_at(&dir, path).is_none(),
            "absent field was materialized: {path}"
        );
        request(&api, Method::GET, &at(path), "", StatusCode::NOT_FOUND).await;
    }
    let sibling = at("routing/retry/max-retry-credentials");
    assert_eq!(
        request(&api, Method::GET, &sibling, "", StatusCode::OK).await,
        "7"
    );

    // Apart from the deletions and the new sibling, the whole document has
    // the same fields and values, nulls and empty mappings included.
    let expected = "config-version: 8
server: {port: 8317}
routing: {retry: {max-retry-credentials: 7}}
plugins:
  configs:
    sample: {enabled: false, options: {}, custom-null: null, custom-tree: {unknown: [one, {two: 2}]}}
";
    let expected = V8Document::migrate(expected.as_bytes()).unwrap();
    let saved = V8Document::migrate(read(&dir).as_bytes()).unwrap();
    assert!(
        saved.value(&[]).map(Result::unwrap) == expected.value(&[]).map(Result::unwrap),
        "DELETE changed unrelated fields or their presence: {}",
        read(&dir)
    );
    let plugin = ["plugins", "configs", "sample"];
    assert!(
        saved.value(&plugin).map(Result::unwrap) == original.value(&plugin).map(Result::unwrap),
        "opaque plugin settings changed"
    );
    let data = read(&dir);
    for comment in [
        "# Keep document comment",
        "# Keep empty mapping",
        "# Keep explicit null",
        "# Keep new sibling",
    ] {
        assert!(data.contains(comment), "lost comment {comment}: {data}");
    }

    for (name, body) in [("options", "{}"), ("custom-null", "null")] {
        let path = at(&format!("plugins/configs/sample/{name}"));
        assert_eq!(
            request(&api, Method::GET, &path, "", StatusCode::OK)
                .await
                .trim(),
            body
        );
        request(&api, Method::DELETE, &path, "", StatusCode::OK).await;
        loaded(&api, &dir);
        request(&api, Method::GET, &path, "", StatusCode::NOT_FOUND).await;
        request(&api, Method::DELETE, &path, "", StatusCode::NOT_FOUND).await;
    }
    request(&api, Method::DELETE, &at(""), "", StatusCode::BAD_REQUEST).await;
}

/// The v8 key groups of upstream's
/// `TestConfigV8APIKeysExposeAuthIndex_Issue6287`.
const AUTH_INDEX: &str = "config-version: 8
port: 8317
api-keys:
  codex:
    - name: codex-group
      base-url: https://api.openai.invalid
      keys:
        - api-key: sk-codex-1
        - api-key: sk-codex-2
  claude:
    - name: claude-group
      base-url: https://api.anthropic.invalid
      keys:
        - api-key: sk-claude-1
  openai-compatibility:
    - name: compat-provider
      base-url: https://api.compat.invalid
      keys:
        - api-key: sk-compat-1
    - name: keyless-provider
      base-url: https://api.keyless.invalid
      keys: []
";

/// Ported from upstream's config_v8_auth_index_test.go
/// (TestConfigV8APIKeysExposeAuthIndex_Issue6287), its cases 5 and 6: the
/// `auth_index` a read adds to the keys isn't written back by a `PUT` of
/// what was read, nor by a `PATCH` that carries one.
#[tokio::test]
async fn config_v8_api_keys_auth_index_is_not_written() {
    let (dir, api) = over(AUTH_INDEX);
    let codex = at("api-keys/codex");
    let groups = request(&api, Method::GET, &codex, "", StatusCode::OK).await;
    assert!(groups.contains("auth_index"), "{groups}");
    request(&api, Method::PUT, &codex, &groups, StatusCode::OK).await;
    let saved = read(&dir);
    assert!(
        !saved.contains("auth_index") && !saved.contains("auth-index"),
        "{saved}"
    );
    assert!(saved.contains("sk-codex-2"), "{saved}");

    let patch = r#"{"api-keys":{"claude":[{"name":"claude-group","base-url":"https://api.anthropic.invalid","keys":[{"api-key":"sk-claude-1","auth_index":"arbitrary-ignore"}]}]}}"#;
    request(&api, Method::PATCH, CONFIG, patch, StatusCode::OK).await;
    let saved = read(&dir);
    assert!(!saved.contains("auth_index"), "{saved}");
    assert!(!saved.contains("arbitrary-ignore"), "{saved}");
    loaded(&api, &dir);
}

/// Ported from upstream's config_v8_auth_index_test.go
/// (TestConfigV8APIKeysExposeAuthIndex_Issue6287), its case 10: only the
/// keys' and groups' own `auth_index` is dropped; one in a plugin's options
/// or in headers is written back.
#[tokio::test]
async fn config_v8_api_keys_auth_index_elsewhere_is_kept() {
    let raw = "config-version: 8
port: 8317
plugins:
  configs:
    custom-plugin:
      enabled: true
      options:
        auth_index: keep-this-plugin-field
api-keys:
  codex:
    - name: codex-group
      base-url: https://api.openai.invalid
      headers:
        auth_index: keep-group-header
      keys:
        - api-key: sk-codex-headers
          headers:
            auth_index: keep-key-header
";
    let (dir, api) = over(raw);
    let codex = at("api-keys/codex");
    let groups = request(&api, Method::GET, &codex, "", StatusCode::OK).await;
    request(&api, Method::PUT, &codex, &groups, StatusCode::OK).await;
    let saved = read(&dir);
    for kept in [
        "keep-this-plugin-field",
        "keep-group-header",
        "keep-key-header",
    ] {
        assert!(saved.contains(kept), "{kept} was stripped: {saved}");
    }
    loaded(&api, &dir);
}

/// Ported from upstream's config_v8_client_test.go
/// (TestConfigV8ClientMultiAgentMigration): the client flag reads and
/// writes at its v8 path and its historical ones, wherever the file has
/// it, and a write that fails leaves the legacy file as it was.
#[tokio::test]
async fn config_v8_client_multi_agent_migration() {
    for raw in [
        "codex: {optimize-multi-agent-v2: true}\n",
        "providers: {codex: {optimize-multi-agent-v2: true}}\n",
        "oauth: {providers: {codex: {optimize-multi-agent-v2: true}}}\n",
    ] {
        let raw = format!("{raw}client: {{codex: {{enable-apply-patch: true}}}}\n");
        let (dir, api) = over(&raw);
        let canonical = at("client/codex/optimize-multi-agent-v2");
        assert_eq!(
            request(&api, Method::GET, &canonical, "", StatusCode::OK).await,
            "true"
        );
        let invalid = r#""invalid""#;
        request(
            &api,
            Method::PUT,
            &canonical,
            invalid,
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
        assert_eq!(
            read(&dir),
            raw,
            "GET or rejected write changed the legacy file"
        );
        for old in ["providers/codex", "oauth/providers/codex", "codex"] {
            let url = at(&format!("{old}/optimize-multi-agent-v2"));
            request(&api, Method::PUT, &url, "true", StatusCode::OK).await;
            let got = request(&api, Method::GET, &url, "", StatusCode::OK).await;
            assert_eq!(got, "true", "{raw}: historical client path {old}");
        }
        for enabled in [false, true] {
            request(
                &api,
                Method::PUT,
                &canonical,
                &enabled.to_string(),
                StatusCode::OK,
            )
            .await;
            let client = api.state.config().client.codex.clone();
            assert!(
                client.optimize_multi_agent_v2 == enabled && client.enable_apply_patch,
                "{raw}: management update changed client settings"
            );
            assert_v8_file(&dir);
            let config = loaded(&api, &dir);
            assert_eq!(config.client.codex.optimize_multi_agent_v2, enabled);
        }
        request(&api, Method::DELETE, &canonical, "", StatusCode::OK).await;
        let client = api.state.config().client.codex.clone();
        assert!(
            !client.optimize_multi_agent_v2 && client.enable_apply_patch,
            "{raw}: delete did not restore default false or changed apply_patch"
        );
        loaded(&api, &dir);
    }
}

/// Ported from upstream's config_v8_upstream_test.go
/// (TestConfigV8SharedUpstreamRoundTrip): a provider setting written at its
/// historical OAuth path reads at `upstream`, writes there keep the OAuth
/// header defaults OAuth-only, and deleting `upstream/claude` restores its
/// defaults.
#[tokio::test]
async fn config_v8_shared_upstream_round_trip() {
    let raw = "oauth: {providers: {codex: {stream-bootstrap-buffering: true, header-defaults: {user-agent: oauth-agent}}}}\n";
    let (dir, api) = over(raw);
    let buffering = at("upstream/codex/stream-bootstrap-buffering");
    assert_eq!(
        request(&api, Method::GET, &buffering, "", StatusCode::OK).await,
        "true"
    );
    assert_eq!(read(&dir), raw, "GET rewrote the historical document");
    request(&api, Method::PUT, &buffering, "false", StatusCode::OK).await;
    let workers = at("oauth/auth-auto-refresh-workers");
    request(&api, Method::PUT, &workers, "3", StatusCode::OK).await;
    let claude = at("upstream/claude");
    let defaults =
        r#"{"header-defaults":{"timezone":"Asia/Singapore","stabilize-device-profile":false}}"#;
    request(&api, Method::PATCH, &claude, defaults, StatusCode::OK).await;
    let saved = read(&dir);
    assert_eq!(
        saved,
        "oauth: {providers: {codex: {header-defaults: {user-agent: oauth-agent}}}, auth-auto-refresh-workers: 3}\nupstream:\n    codex:\n        stream-bootstrap-buffering: false\n    claude:\n        \"header-defaults\": {\"timezone\": \"Asia/Singapore\", \"stabilize-device-profile\": false}\nconfig-version: 8\n"
    );
    let config = loaded(&api, &dir);
    assert!(
        !config.codex.stream_bootstrap_buffering && config.auth_auto_refresh_workers == 3,
        "management write lost shared upstream values"
    );
    assert_eq!(
        value_at(&dir, "upstream/claude/header-defaults/timezone"),
        Some(AnyValue::Str("Asia/Singapore".into()))
    );
    // The user agent stays OAuth-only.
    assert_eq!(
        value_at(&dir, "oauth/providers/codex/header-defaults/user-agent"),
        Some(AnyValue::Str("oauth-agent".into()))
    );
    assert!(value_at(&dir, "upstream/codex/header-defaults").is_none());

    let historical = at("oauth/providers/codex/stream-bootstrap-buffering");
    let invalid = r#""invalid""#;
    request(
        &api,
        Method::PUT,
        &historical,
        invalid,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert_eq!(
        read(&dir),
        saved,
        "rejected historical-path write changed the document"
    );
    request(&api, Method::DELETE, &claude, "", StatusCode::OK).await;
    let missing = request(&api, Method::GET, &claude, "", StatusCode::NOT_FOUND).await;
    assert!(!missing.is_empty(), "missing subtree response was empty");
    assert!(value_at(&dir, "upstream/claude").is_none());
    loaded(&api, &dir);
}

/// Ported from upstream's config_v8_compatibility_test.go
/// (TestConfigV8HistoricalFieldPaths): each setting reads and writes at its
/// historical path as at its current one, an explicit null included, and
/// is saved at its current path.
#[tokio::test]
async fn config_v8_historical_field_paths() {
    for (historical, current, value) in [
        (
            "oauth/providers/codex/disable-codex-cloaking",
            "upstream/codex/disable-codex-cloaking",
            "true",
        ),
        (
            "oauth/providers/codex/stream-bootstrap-buffering",
            "upstream/codex/stream-bootstrap-buffering",
            "true",
        ),
        (
            "oauth/providers/codex/stream-bootstrap-timeout",
            "upstream/codex/stream-bootstrap-timeout",
            r#""10s""#,
        ),
        (
            "oauth/providers/codex/orphan-delegation-compatibility",
            "upstream/codex/orphan-delegation-compatibility",
            "true",
        ),
        (
            "oauth/providers/codex/model-level-cooling",
            "upstream/codex/model-level-cooling",
            "true",
        ),
        (
            "oauth/providers/codex/response-steering",
            "upstream/codex/response-steering",
            "true",
        ),
        (
            "oauth/providers/claude/model-level-cooling",
            "upstream/claude/model-level-cooling",
            "true",
        ),
        (
            "oauth/providers/claude/disable-claude-cloak-mode",
            "upstream/claude/disable-claude-cloak-mode",
            "true",
        ),
        (
            "oauth/providers/claude/header-defaults/user-agent",
            "upstream/claude/header-defaults/user-agent",
            r#""test-agent""#,
        ),
        (
            "oauth/providers/claude/header-defaults/package-version",
            "upstream/claude/header-defaults/package-version",
            r#""0.2.0""#,
        ),
        (
            "oauth/providers/claude/header-defaults/runtime-version",
            "upstream/claude/header-defaults/runtime-version",
            r#""v22""#,
        ),
        (
            "oauth/providers/claude/header-defaults/os",
            "upstream/claude/header-defaults/os",
            r#""Windows""#,
        ),
        (
            "oauth/providers/claude/header-defaults/arch",
            "upstream/claude/header-defaults/arch",
            r#""amd64""#,
        ),
        (
            "oauth/providers/claude/header-defaults/timeout",
            "upstream/claude/header-defaults/timeout",
            r#""300""#,
        ),
        (
            "oauth/providers/claude/header-defaults/timezone",
            "upstream/claude/header-defaults/timezone",
            r#""Asia/Shanghai""#,
        ),
        (
            "oauth/providers/claude/header-defaults/stabilize-device-profile",
            "upstream/claude/header-defaults/stabilize-device-profile",
            "true",
        ),
        (
            "oauth/providers/claude/claude-code/disable-cloaking-model-list",
            "upstream/claude/disable-cloaking-model-list",
            "false",
        ),
        (
            "oauth/providers/xai/inject-x-search",
            "upstream/xai/inject-x-search",
            "true",
        ),
        (
            "oauth/providers/codex/optimize-multi-agent-v2",
            "client/codex/optimize-multi-agent-v2",
            "true",
        ),
    ] {
        let (dir, api) = over("server: {port: 8317}\n");
        let (historical, current) = (at(historical), at(current));
        request(&api, Method::PUT, &current, value, StatusCode::OK).await;
        let got = request(&api, Method::GET, &historical, "", StatusCode::OK).await;
        assert_eq!(got, value, "{historical}");
        request(&api, Method::PATCH, &historical, value, StatusCode::OK).await;
        request(&api, Method::PUT, &historical, "null", StatusCode::OK).await;
        let got = request(&api, Method::GET, &current, "", StatusCode::OK).await;
        assert_eq!(
            got, "null",
            "{historical}: historical PUT lost explicit null"
        );
        request(&api, Method::DELETE, &historical, "", StatusCode::OK).await;
        request(&api, Method::GET, &current, "", StatusCode::NOT_FOUND).await;
        let port = request(&api, Method::GET, &at("server/port"), "", StatusCode::OK).await;
        assert_eq!(
            port, "8317",
            "{historical}: DELETE changed unrelated settings"
        );
        assert_v8_file(&dir);
        loaded(&api, &dir);
    }
}

/// Ported from upstream's config_v8_compatibility_test.go
/// (TestConfigV8HistoricalProviderSubtrees): a provider's historical OAuth
/// subtree shows its shared and client settings, and a `PATCH`, `PUT` or
/// `DELETE` of it merges, replaces or removes them all, leaving other
/// providers and the OAuth-only header defaults as they were.
#[tokio::test]
async fn config_v8_historical_provider_subtrees() {
    let raw = "upstream: {codex: {response-steering: true, stream-bootstrap-buffering: true}, xai: {inject-x-search: true}}\noauth: {providers: {codex: {header-defaults: {user-agent: oauth-agent}}}}\nclient: {codex: {optimize-multi-agent-v2: true, enable-apply-patch: true}}\n";
    for (method, path, body, steering, buffering, optimize, agent) in [
        (
            Method::PATCH,
            "oauth/providers/codex",
            r#"{"response-steering":false,"header-defaults":{"user-agent":"updated"}}"#,
            false,
            true,
            true,
            Some("updated"),
        ),
        (
            Method::PUT,
            "oauth/providers/codex",
            r#"{"response-steering":true}"#,
            true,
            false,
            false,
            None,
        ),
        (
            Method::DELETE,
            "oauth/providers/codex",
            "",
            false,
            false,
            false,
            None,
        ),
        (
            Method::PUT,
            "upstream/codex",
            r#"{"response-steering":false}"#,
            false,
            false,
            true,
            Some("oauth-agent"),
        ),
    ] {
        let (dir, api) = over(raw);
        let view = request(
            &api,
            Method::GET,
            &at("oauth/providers/codex"),
            "",
            StatusCode::OK,
        )
        .await;
        let provider: Value = serde_json::from_str(&view).unwrap();
        assert!(
            provider["response-steering"] == true && provider["optimize-multi-agent-v2"] == true,
            "historical provider view omitted shared/client values: {view}"
        );
        assert_eq!(read(&dir), raw, "historical GET changed the file");
        request(&api, method.clone(), &at(path), body, StatusCode::OK).await;
        let config = loaded(&api, &dir);
        let case = format!("{method} {path}");
        assert_eq!(
            (
                config.codex.response_steering,
                config.codex.stream_bootstrap_buffering,
                config.client.codex.optimize_multi_agent_v2,
            ),
            (steering, buffering, optimize),
            "{case}: replacement or merge semantics not kept"
        );
        assert_eq!(
            value_at(&dir, "oauth/providers/codex/header-defaults/user-agent"),
            agent.map(|agent| AnyValue::Str(agent.into())),
            "{case}"
        );
        assert!(
            config.xai.inject_x_search && config.client.codex.enable_apply_patch,
            "{case}: changed unrelated settings"
        );
        assert!(
            value_at(&dir, "upstream/codex/header-defaults").is_none(),
            "{case}: changed the OAuth scope"
        );
        assert_v8_file(&dir);
    }
}

/// Ported from upstream's config_v8_compatibility_test.go
/// (TestConfigV8HistoricalConfigurationBodies): a whole-config body using
/// historical paths is saved at the current ones, where the current path
/// wins, and YAML keeps its comments.
#[tokio::test]
async fn config_v8_historical_configuration_bodies() {
    for (method, route, body, steering, buffering) in [
        (
            Method::PATCH,
            "config",
            r#"{"oauth":{"providers":{"codex":{"response-steering":false}}}}"#,
            false,
            true,
        ),
        (
            Method::PATCH,
            "config",
            r#"{"oauth":{"providers":{"codex":{"response-steering":true}}},"upstream":{"codex":{"response-steering":false}}}"#,
            false,
            true,
        ),
        (
            Method::PATCH,
            "config",
            r#"{"oauth":{"providers":{"codex":{"response-steering":true}}},"upstream":{"codex":{"response-steering":null}}}"#,
            false,
            true,
        ),
        (
            Method::PUT,
            "config",
            r#"{"oauth":{"providers":{"codex":{"response-steering":true}}}}"#,
            true,
            false,
        ),
        (
            Method::PUT,
            "config.yaml",
            "# Keep this comment\noauth: {providers: {codex: {response-steering: false}}}\n",
            false,
            false,
        ),
    ] {
        let raw =
            "upstream: {codex: {response-steering: true, stream-bootstrap-buffering: true}}\n";
        let (dir, api) = over(raw);
        let route = format!("/v8/management/{route}");
        request(&api, method, &route, body, StatusCode::OK).await;
        let data = assert_v8_file(&dir);
        let config = loaded(&api, &dir);
        assert_eq!(
            (
                config.codex.response_steering,
                config.codex.stream_bootstrap_buffering
            ),
            (steering, buffering),
            "{body}: historical body lost effective values"
        );
        if route.ends_with(".yaml") {
            assert!(
                data.contains("# Keep this comment"),
                "YAML alias normalization lost comments"
            );
        }
    }
}

/// Ported from upstream's config_v8_compatibility_test.go
/// (TestLegacyConfigYAMLSavesBySubmittedVersion): `PUT config.yaml` writes
/// a legacy body as sent, and a v8 or mixed one in the current v8 layout,
/// with its comments.
#[tokio::test]
async fn legacy_config_yaml_saves_by_submitted_version() {
    for (body, steering, v8) in [
        (
            "# Keep this comment\ncodex: {response-steering: true}\n",
            true,
            false,
        ),
        (
            "# Keep this comment\noauth: {providers: {codex: {response-steering: true}}}\n",
            true,
            true,
        ),
        (
            "# Keep this comment\nconfig-version: 8\nupstream: {codex: {response-steering: false}}\n",
            false,
            true,
        ),
        (
            "# Keep this comment\nrequest-retry: 3\nupstream: {codex: {response-steering: false}}\n",
            false,
            true,
        ),
    ] {
        let (dir, api) = over("server: {port: 8317}\n");
        let answer = request(
            &api,
            Method::PUT,
            "/v0/management/config.yaml",
            body,
            StatusCode::OK,
        )
        .await;
        assert_eq!(answer, r#"{"changed":["config"],"ok":true}"#);
        let data = read(&dir);
        if v8 {
            assert_v8_file(&dir);
        } else {
            assert_eq!(data, body, "v0 changed the submitted layout");
        }
        assert!(
            data.contains("# Keep this comment"),
            "v0 YAML write lost comments"
        );
        let config = loaded(&api, &dir);
        let on = config.codex.response_steering;
        assert_eq!(
            for_api_key(&config, "codex.response-steering", on),
            steering,
            "{body}: changed shared runtime values"
        );
    }
}

/// Ported from upstream's config_v8_compatibility_test.go
/// (TestV0SetterSavesByExistingVersion): a v0 setting is saved in the
/// file's own layout, moving a historical v8 file to the current one, and
/// leaves the other settings as they were.
#[tokio::test]
async fn v0_setter_saves_by_existing_version() {
    for (raw, v8) in [
        (
            "request-retry: 3\ncodex: {response-steering: true}\nxai: {inject-x-search: true}\nrouting: {strategy: fill-first}\n",
            false,
        ),
        (
            "config-version: 8\noauth: {providers: {codex: {response-steering: true, header-defaults: {user-agent: oauth-agent}}, xai: {inject-x-search: true}}}\n",
            true,
        ),
        (
            "oauth: {providers: {codex: {response-steering: true}, xai: {inject-x-search: true}}}\n",
            true,
        ),
        (
            "config-version: 8\nupstream: {codex: {response-steering: true}, xai: {inject-x-search: true}}\nrouting: {retry: {request-retry: 3}}\n",
            true,
        ),
    ] {
        let (dir, api) = over(raw);
        let agent = at("oauth/providers/codex/header-defaults/user-agent");
        let before = api.get(&agent).await;
        request(
            &api,
            Method::PUT,
            "/v0/management/request-retry",
            r#"{"value":2}"#,
            StatusCode::OK,
        )
        .await;
        let data = read(&dir);
        if v8 {
            assert_v8_file(&dir);
        } else {
            assert!(
                !data.contains("config-version:") && !data.contains("upstream:"),
                "v0 migrated a legacy-only file: {data}"
            );
        }
        let config = loaded(&api, &dir);
        assert_eq!(config.request_retry, 2, "{raw}");
        let steering = config.codex.response_steering;
        let inject = config.xai.inject_x_search;
        assert!(
            for_api_key(&config, "codex.response-steering", steering)
                && for_api_key(&config, "xai.inject-x-search", inject),
            "{raw}: v0 save changed effective settings"
        );
        let after = api.get(&agent).await;
        assert_eq!(
            (after.status, after.body),
            (before.status, before.body),
            "{raw}"
        );
    }
}

/// Ported from upstream's config_v8_compatibility_test.go
/// (TestConfigV8HistoricalNullContainerPatch): a null header-defaults
/// container at its historical path resets each header default, and keeps
/// the provider's other settings.
#[tokio::test]
async fn config_v8_historical_null_container_patch() {
    let raw = "upstream: {claude: {model-level-cooling: true, header-defaults: {user-agent: agent, timezone: UTC, stabilize-device-profile: true}}}\n";
    let (dir, api) = over(raw);
    let body = r#"{"oauth":{"providers":{"claude":{"header-defaults":null}}}}"#;
    request(&api, Method::PATCH, CONFIG, body, StatusCode::OK).await;
    assert_eq!(
        read(&dir),
        "upstream: {claude: {model-level-cooling: true, header-defaults: {user-agent: null, timezone: null, stabilize-device-profile: null, package-version: null, runtime-version: null, os: null, arch: null, timeout: null}}}\nconfig-version: 8\n"
    );
    let config = loaded(&api, &dir);
    assert!(config.claude.model_level_cooling, "changed cooling");
    for field in ["user-agent", "timezone"] {
        let path = format!("upstream/claude/header-defaults/{field}");
        assert_eq!(value_at(&dir, &path), Some(AnyValue::Null), "{field}");
    }
}

/// Ported from upstream's config_v8_compatibility_test.go
/// (TestV0RepeatedSavesKeepLegacyClientAlias): repeated v0 saves keep a
/// legacy file's layout, its client flag at the legacy path, and each
/// comment once.
#[tokio::test]
async fn v0_repeated_saves_keep_legacy_client_alias() {
    let raw = "# LEGACY DOCUMENT\ncodex:\n  # OPTIMIZE HEAD\n  optimize-multi-agent-v2: true # OPTIMIZE INLINE\n  response-steering: true\nrequest-retry: 3\n";
    let (dir, api) = over(raw);
    for body in [r#"{"value":2}"#, r#"{"value":1}"#, r#"{"value":0}"#] {
        request(
            &api,
            Method::PUT,
            "/v0/management/request-retry",
            body,
            StatusCode::OK,
        )
        .await;
        let data = read(&dir);
        assert!(
            !data.contains("config-version:")
                && !data.contains("upstream:")
                && !data.contains("client:")
                && data.contains("optimize-multi-agent-v2: true"),
            "v0 save changed the legacy layout: {data}"
        );
        for marker in ["LEGACY DOCUMENT", "OPTIMIZE HEAD", "OPTIMIZE INLINE"] {
            assert_eq!(data.matches(marker).count(), 1, "{marker}: {data}");
        }
        let config = loaded(&api, &dir);
        let steering = config.codex.response_steering;
        assert!(
            config.client.codex.optimize_multi_agent_v2
                && for_api_key(&config, "codex.response-steering", steering),
            "v0 save changed effective settings"
        );
    }
}

/// Ported from upstream's config_v8_compatibility_test.go
/// (TestConfigV8FieldUpdatesKeepComments): a field's comments, and its
/// provider's, stay once through updates at its current and historical
/// paths.
#[tokio::test]
async fn config_v8_field_updates_keep_comments() {
    for method in [Method::PUT, Method::PATCH] {
        for path in [
            "oauth/providers/codex/response-steering",
            "upstream/codex/response-steering",
        ] {
            let raw = "oauth:\n  providers:\n    codex: # PROVIDER INLINE\n      # FIELD HEAD\n      response-steering: true # FIELD INLINE\n\n      # FIELD FOOT\n";
            let (dir, api) = over(raw);
            for body in ["false", "null", "true"] {
                request(&api, method.clone(), &at(path), body, StatusCode::OK).await;
                let data = read(&dir);
                for marker in [
                    "PROVIDER INLINE",
                    "FIELD HEAD",
                    "FIELD INLINE",
                    "FIELD FOOT",
                ] {
                    let count = data.matches(marker).count();
                    assert_eq!(count, 1, "{method} {path} {body}: {marker}: {data}");
                }
                loaded(&api, &dir);
            }
        }
    }
}

/// Ported from upstream's config_priority_test.go
/// (TestPatchPriorityForEveryProvider) and config_claude_key_test.go
/// (TestPatchClaudeKeyPriority): a key's priority set with a `PATCH` is
/// saved, kept by a `PATCH` that doesn't name it, and reset by one that
/// sets it to 0. (The configs these patches make are checked in
/// `config_keys`.)
#[tokio::test]
async fn patched_priority_is_saved() {
    for (list, entry) in [
        ("claude-api-key", "api-key: key"),
        (
            "xai-api-key",
            "api-key: key\n    base-url: https://example.invalid",
        ),
        (
            "meta-api-key",
            "api-key: key\n    base-url: https://example.invalid",
        ),
        (
            "codex-api-key",
            "api-key: key\n    base-url: https://example.invalid",
        ),
        ("gemini-api-key", "api-key: key"),
        ("interactions-api-key", "api-key: key"),
        (
            "vertex-api-key",
            "api-key: key\n    base-url: https://example.invalid",
        ),
        (
            "openai-compatibility",
            "name: compat\n    base-url: https://compat.example.invalid",
        ),
    ] {
        let raw = format!("{list}:\n  - {entry}\n");
        let (dir, api) = over(&raw);
        let path = format!("/v0/management/{list}");
        for (value, saved) in [
            (r#"{"priority":7}"#, true),
            (r#"{"prefix":"team-a"}"#, true),
            (r#"{"priority":0}"#, false),
        ] {
            let body = format!(r#"{{"index":0,"value":{value}}}"#);
            request(&api, Method::PATCH, &path, &body, StatusCode::OK).await;
            let data = read(&dir);
            assert_eq!(
                data.contains("priority: 7"),
                saved,
                "{list} {value}: {data}"
            );
            loaded(&api, &dir);
        }
    }

    let raw = "claude-api-key:\n  - api-key: key-0\n  - api-key: key-1\n    priority: 5\n";
    let (dir, api) = over(raw);
    let path = "/v0/management/claude-api-key";
    let body = r#"{"index":1,"value":{"priority":20}}"#;
    request(&api, Method::PATCH, path, body, StatusCode::OK).await;
    let data = read(&dir);
    assert!(
        data.contains("priority: 20") && !data.contains("priority: 5"),
        "{data}"
    );
    assert_eq!(loaded(&api, &dir).claude_api_key[1].priority, 20);
}

/// A Claude key carrying the client impersonation settings this port
/// doesn't type.
const CLOAKED: &str = "port: 8317
claude-api-key:
  - api-key: sk-a
    base-url: https://a.invalid
    cloak:
      mode: always # keep cloak
      strict-mode: true
      sensitive-words: [alpha, beta]
    fingerprint-profile: claude-code-cli
  - api-key: sk-b
";

/// [`CLOAKED`] with a third key added, as upstream writes it when the list
/// it is sent carries the first key's `cloak` and `fingerprint-profile`.
const CLOAKED_ADDED: &str = r#"port: 8317
claude-api-key:
  - api-key: sk-a
    base-url: https://a.invalid
    cloak:
      mode: always # keep cloak
      strict-mode: true
      sensitive-words:
        - alpha
        - beta
    fingerprint-profile: claude-code-cli
  - api-key: sk-b
  - api-key: sk-c
    base-url: ""
    proxy-url: ""
    models: []
credential-concurrency:
  cpa-heartbeat-timeout: 3s
  cpa-cancel-bound: 5s
  reclaim-grace: 5s
  cleanup-interval: 5s
  release-flush-interval: 250ms
  release-max-backoff: 2s
  busy-retry-min: 250ms
  busy-retry-max: 1s
  max-limit: 1000000
credential-in-flight:
  snapshot-interval: 2s
  stale-after: 10s
  max-part-bytes: 262144
  max-part-count: 64
  max-revision-bytes: 16777216
  max-aggregate-groups: 100000
  max-details: 10000
  max-string-bytes: 256
  staging-retention: 1m
discovery:
  service-type: _ai-gateway._tcp
  subtypes:
    - _chat-completions
    - _responses
    - _messages
    - _generate-content
    - _interactions
redis-usage-queue-retention-seconds: 60
disable-cooling: false
request-retry: 0
ws-auth: true
"#;

// Not upstream's: the dashboard adds a key by reading the list and writing
// it back with the key added. The list read here leaves out the client
// impersonation settings, so the list written back has none, and the file
// keeps the ones it has: it is the file upstream writes when the list
// carries them back, as upstream's read shows them. (Upstream, sent the
// list without them, keeps only the cloak's `mode`.)
#[tokio::test]
async fn adding_a_claude_key_keeps_the_cloak_settings() {
    let (dir, api) = over(CLOAKED);
    let list = request(
        &api,
        Method::GET,
        "/v0/management/claude-api-key",
        "",
        StatusCode::OK,
    )
    .await;
    assert!(
        !list.contains("cloak") && !list.contains("fingerprint"),
        "{list}"
    );
    let list: Value = serde_json::from_str(&list).unwrap();
    let Value::Array(mut keys) = list["claude-api-key"].clone() else {
        panic!("not a list: {list}");
    };
    keys.push(json!({"api-key": "sk-c"}));
    let body = Value::Array(keys).to_string();
    let answer = request(
        &api,
        Method::PUT,
        "/v0/management/claude-api-key",
        &body,
        StatusCode::OK,
    )
    .await;
    assert_eq!(answer, OK);
    assert_eq!(read(&dir), CLOAKED_ADDED);
    let config = loaded(&api, &dir);
    let keys: Vec<&str> = config
        .claude_api_key
        .iter()
        .map(|key| key.api_key.as_str())
        .collect();
    assert_eq!(keys, ["sk-a", "sk-b", "sk-c"]);
}

// Not upstream's: a write the writer fails answers 500 and changes neither
// the file nor the config the handlers read. Here the backup can't be
// written, as a directory is where it goes.
#[tokio::test]
async fn a_failed_write_changes_nothing() {
    let raw = "port: 8317\nrequest-retry: 3\n";
    let (dir, api) = over(raw);
    let backup = dir.config_path().with_file_name("config.yaml.bak");
    std::fs::create_dir(&backup).unwrap();
    let before = api.state.config();
    for (method, path, body, error) in [
        (
            Method::PUT,
            "/v0/management/request-retry",
            r#"{"value":2}"#,
            "failed to save config: ",
        ),
        (
            Method::PATCH,
            CONFIG,
            r#"{"routing":{"retry":{"request-retry":2}}}"#,
            "write_failed",
        ),
        (
            Method::PUT,
            &at("routing/retry/request-retry"),
            "2",
            "write_failed",
        ),
        (
            Method::PUT,
            "/v0/management/config.yaml",
            "port: 8318\n",
            "write_failed",
        ),
    ] {
        let answer = api.call(method.clone(), path, body).await;
        let answer_error = answer.expect(StatusCode::INTERNAL_SERVER_ERROR)["error"].clone();
        let answer_error = answer_error.as_str().unwrap();
        assert!(
            answer_error.starts_with(error),
            "{method} {path}: {answer_error}"
        );
        assert_eq!(read(&dir), raw, "{method} {path}: the file changed");
        assert!(
            *api.state.config() == *before,
            "{method} {path}: the config the handlers read changed"
        );
    }
    std::fs::remove_dir(&backup).unwrap();
    request(
        &api,
        Method::PUT,
        "/v0/management/request-retry",
        r#"{"value":2}"#,
        StatusCode::OK,
    )
    .await;
    assert_eq!(loaded(&api, &dir).request_retry, 2);
}
