// Ported from CLIProxyAPI internal/api/handlers/management/config_basic.go
// (GetConfig, GetConfigYAML, GetDebug, GetLoggingToFile, GetRequestLog,
// GetWebsocketAuth, GetRequestRetry, GetMaxRetryCredentials,
// GetMaxRetryInterval, GetForceModelPrefix, normalizeRoutingStrategy,
// GetRoutingStrategy, GetProxyURL), quota.go (GetSwitchProject,
// GetSwitchPreviewModel), config_lists.go (GetAPIKeys, GetGeminiKeys,
// GetClaudeKeys, GetCodexKeys, GetOpenAICompat, GetVertexCompatKeys,
// GetOAuthExcludedModels, GetOAuthModelAlias, GetOAuthRequestScopedErrors)
// and config_auth_index.go (liveAuthIndexByID and the `*WithAuthIndex`
// lists) (v8.0.10, MIT), and config_v8.go (ConfigV8's reads) (v8.0.11,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Reading the config: as JSON, as the file it was loaded from, one
//! setting or list at a time, and in the v8 layout.
//!
//! - `GET /v0/management/config` writes the config as JSON.
//! - `GET /v0/management/config.yaml` sends the config file as it is.
//! - `GET /v0/management/<setting>` gives one setting as
//!   `{"<name>":<value>}`: `debug`, `logging-to-file`, `proxy-url`,
//!   `quota-exceeded/switch-project`, `quota-exceeded/switch-preview-model`,
//!   `request-log`, `ws-auth`, `request-retry`, `max-retry-credentials`,
//!   `max-retry-interval`, `force-model-prefix` and `routing/strategy`.
//! - `GET /v0/management/<list>` gives one list: `api-keys`,
//!   `gemini-api-key`, `claude-api-key`, `codex-api-key`,
//!   `openai-compatibility`, `vertex-api-key`, `oauth-excluded-models`,
//!   `oauth-model-alias` and `oauth-request-scoped-errors`. Each provider
//!   key carries the `auth-index` of the credential it made, when the
//!   manager holds that credential.
//! - `GET /v8/management/config`, `config/*path` and `config.yaml` read the
//!   config file in the v8 layout: the whole of it or the value at a path
//!   such as `config/server/tls`, as JSON, or the file.
//!
//! The config is only ever read: open-ferry never writes it, so its `PUT`,
//! `PATCH` and `DELETE` routes stay unported, and answer with the empty
//! 404.
//!
//! The reads show the secrets the config holds as upstream shows them, to
//! whoever has the management key: the client API keys, the providers' API
//! keys, the proxy URLs with their passwords, and the whole file from
//! `config.yaml`. Only the v8 JSON reads leave out the ICE servers'
//! usernames and credentials, as upstream's do, and show the management
//! key hashed.
//!
//! Deviations from upstream:
//! - `GET /v0/management/config` writes only the sections open-ferry
//!   types (see [`open_ferry_core::config`]). Left out: `plugins`, `pprof`,
//!   `discovery`, `commercial-mode`, `credential-concurrency`,
//!   `credential-in-flight`, `logs-max-total-size-mb`,
//!   `error-logs-max-files`, `usage-statistics-enabled`,
//!   `redis-usage-queue-retention-seconds`, `save-cooldown-status`,
//!   `disable-image-generation`, `gpt-image-2-base-model`,
//!   `video-result-auth-cache-ttl`, `payload`, the other providers'
//!   sections (`interactions-api-key`, `xai-api-key`, `meta-api-key`,
//!   `xai`, `antigravity`, `antigravity-signature-*`, `devin`),
//!   `codex.live-media-relay`, and the client impersonation settings
//!   (`claude-code`, `claude-header-defaults`, `disable-claude-cloak-mode`,
//!   `codex-header-defaults.user-agent`, `codex.disable-codex-cloaking`, and
//!   each Claude key's `cloak`, `fingerprint-profile` and
//!   `experimental-cch-signing`). A list the file gives as `[]` is written
//!   as `null`.
//! - `api-keys` gives `null` for an empty list where the file has `[]`;
//!   upstream gives `[]`.
//! - The v8 reads see the management key as a bcrypt hash, as upstream
//!   does once it has hashed a plain key into the file. Here the file
//!   keeps the plain key, so the hash is made when it is read (once per
//!   key while the process runs) and differs from one run to the next. A
//!   key over 72 bytes, which upstream refuses to load, is hashed from its
//!   first 72.
//! - `GET /v8/management/config.yaml` sends the file as it is, where
//!   upstream sends it migrated to the v8 layout. So it shows the
//!   management key as the file holds it.
//! - A read error's message is the operating system's, as Rust words it.
//! - A file that doesn't load is `invalid_config` with the message
//!   open-ferry's config loader gives: a syntax error is worded as the
//!   saphyr parser words it (`yaml: line 2: while parsing a node, did not
//!   find expected node content`, where upstream has `yaml: line 1: did
//!   not find expected node content`), and a type error doesn't quote the
//!   value (`cannot decode !!str as a !!int`).

mod config_json;

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get};
use http::{HeaderValue, StatusCode, header};
use open_ferry_core::auth::synthesizer::{StableIdGenerator, format_sorted_headers};
use open_ferry_core::config::{AnyValue, Config, V8Document};
use open_ferry_translate::go::to_lower;

pub(crate) use config_json::{Fields, strings, thinking_fields};

use crate::Route;
use crate::auth_files::{auth_index, run_blocking};
use crate::json::{self, Json, format_float, write_string};
use crate::state::ManagementState;

/// What `Content-Type` a config file is sent with.
const YAML_CONTENT_TYPE: &str = "application/yaml; charset=utf-8";

/// How a setting's value is read from the config.
type Read = fn(&Config) -> Json;

/// The single settings: path under `/v0/management/`, the name the answer
/// gives it, and its value.
const SETTINGS: &[(&str, &str, Read)] = &[
    ("debug", "debug", |config| Json::Bool(config.debug)),
    ("logging-to-file", "logging-to-file", |config| {
        Json::Bool(config.logging_to_file)
    }),
    ("proxy-url", "proxy-url", |config| {
        Json::Str(config.proxy_url.clone())
    }),
    (
        "quota-exceeded/switch-project",
        "switch-project",
        |config| Json::Bool(config.quota_exceeded.switch_project),
    ),
    (
        "quota-exceeded/switch-preview-model",
        "switch-preview-model",
        |config| Json::Bool(config.quota_exceeded.switch_preview_model),
    ),
    ("request-log", "request-log", |config| {
        Json::Bool(config.request_log)
    }),
    ("ws-auth", "ws-auth", |config| Json::Bool(config.ws_auth)),
    ("request-retry", "request-retry", |config| {
        Json::Int(config.request_retry)
    }),
    ("max-retry-credentials", "max-retry-credentials", |config| {
        Json::Int(config.max_retry_credentials)
    }),
    ("max-retry-interval", "max-retry-interval", |config| {
        Json::Int(config.max_retry_interval)
    }),
    ("force-model-prefix", "force-model-prefix", |config| {
        Json::Bool(config.force_model_prefix)
    }),
    ("routing/strategy", "strategy", |config| {
        Json::Str(routing_strategy(&config.routing.strategy))
    }),
];

/// The routes this module serves.
pub(crate) fn routes() -> Vec<Route> {
    let mut routes = vec![
        Route::key("/v0/management/config", get(get_config)),
        Route::key("/v0/management/config.yaml", get(get_config_yaml)),
        Route::key("/v0/management/api-keys", get(api_keys)),
        Route::key("/v0/management/gemini-api-key", get(gemini_keys)),
        Route::key("/v0/management/claude-api-key", get(claude_keys)),
        Route::key("/v0/management/codex-api-key", get(codex_keys)),
        Route::key(
            "/v0/management/openai-compatibility",
            get(openai_compatibility),
        ),
        Route::key("/v0/management/vertex-api-key", get(vertex_keys)),
        Route::key(
            "/v0/management/oauth-excluded-models",
            get(oauth_excluded_models),
        ),
        Route::key("/v0/management/oauth-model-alias", get(oauth_model_alias)),
        Route::key(
            "/v0/management/oauth-request-scoped-errors",
            get(oauth_request_scoped_errors),
        ),
        Route::key("/v8/management/config", get(config_v8)),
        Route::key("/v8/management/config.yaml", get(config_v8_yaml)),
        // `{*path}` doesn't match an empty rest, which upstream's `*path`
        // does.
        Route::key("/v8/management/config/", get(config_v8)),
        Route::key("/v8/management/config/{*path}", get(config_v8_path)),
    ];
    routes.extend(SETTINGS.iter().map(|&(path, name, read)| {
        Route::key(format!("/v0/management/{path}"), setting(name, read))
    }));
    routes
}

/// A getter answering `{"<name>":<value>}`.
fn setting(name: &'static str, read: Read) -> MethodRouter<ManagementState> {
    get(move |State(state): State<ManagementState>| async move {
        json::response(StatusCode::OK, &Json::map([(name, read(&state.config()))]))
    })
}

/// A routing strategy's canonical name, or the setting trimmed when it
/// names none (upstream's `normalizeRoutingStrategy` as `GetRoutingStrategy`
/// uses it).
fn routing_strategy(raw: &str) -> String {
    match to_lower(raw.trim()).as_str() {
        "" | "round-robin" | "roundrobin" | "rr" => "round-robin".to_owned(),
        "weighted-round-robin" | "weightedroundrobin" | "wrr" => "weighted-round-robin".to_owned(),
        "fill-first" | "fillfirst" | "ff" => "fill-first".to_owned(),
        _ => raw.trim().to_owned(),
    }
}

/// `GET /v0/management/config` (upstream's `GetConfig`).
async fn get_config(State(state): State<ManagementState>) -> Response {
    json::response(StatusCode::OK, &config_json::config(&state.config()))
}

/// `GET /v0/management/config.yaml` (upstream's `GetConfigYAML`): the file
/// as it is.
async fn get_config_yaml(State(state): State<ManagementState>) -> Response {
    let path = state.config_path().map(PathBuf::from);
    let read = run_blocking(move || match path {
        Some(path) => std::fs::read(path),
        None => Err(ErrorKind::NotFound.into()),
    })
    .await;
    match read {
        Ok(data) => {
            let mut response = yaml_response(data);
            response.headers_mut().insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            response
        }
        Err(error) if error.kind() == ErrorKind::NotFound => json::response(
            StatusCode::NOT_FOUND,
            &Json::map([
                ("error", Json::Str("not_found".into())),
                ("message", Json::Str("config file not found".into())),
            ]),
        ),
        Err(error) => json::response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &Json::map([
                ("error", Json::Str("read_failed".into())),
                ("message", Json::Str(error.to_string())),
            ]),
        ),
    }
}

/// A config file, sent as it is, not to be cached.
fn yaml_response(data: Vec<u8>) -> Response {
    let mut response = (StatusCode::OK, data).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(YAML_CONTENT_TYPE),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `GET /v0/management/api-keys` (upstream's `GetAPIKeys`).
async fn api_keys(State(state): State<ManagementState>) -> Response {
    let config = state.config();
    let keys = if config.api_keys.is_empty() {
        Json::Null
    } else {
        Json::Array(config.api_keys.iter().cloned().map(Json::Str).collect())
    };
    json::response(StatusCode::OK, &Json::map([("api-keys", keys)]))
}

/// Finds the credential each config entry made, as upstream's
/// `*WithAuthIndex` lists do: the entries' IDs are made again, in order,
/// and looked up among the credentials the manager holds.
struct Indexes {
    /// Each credential's index by its trimmed ID (upstream's
    /// `liveAuthIndexByID`).
    live: HashMap<String, String>,
    ids: StableIdGenerator,
}

impl Indexes {
    fn new(state: &ManagementState) -> Self {
        let live = state
            .manager()
            .list()
            .iter()
            .filter_map(|auth| {
                let id = auth.id.trim();
                let index = auth_index(auth);
                (!id.is_empty() && !index.is_empty()).then(|| (id.to_owned(), index))
            })
            .collect();
        Self {
            live,
            ids: StableIdGenerator::new(),
        }
    }

    /// The index of the credential with the next ID for `kind` and
    /// `parts`, or empty when the manager holds none.
    fn next(&mut self, kind: &str, parts: &[&str]) -> String {
        let (id, _) = self.ids.next(kind, parts);
        self.live.get(&id).cloned().unwrap_or_default()
    }

    /// The index for a Gemini, Claude or Codex key, from its key, base URL,
    /// proxy URL and prefix and its headers; empty, without taking an ID,
    /// for an entry with neither a key nor a base URL.
    fn api_key(
        &mut self,
        provider: &str,
        [key, base_url, proxy_url, prefix]: [&str; 4],
        headers: &std::collections::BTreeMap<String, String>,
    ) -> String {
        let key = key.trim();
        let base_url = base_url.trim();
        if key.is_empty() && base_url.is_empty() {
            return String::new();
        }
        let headers = format_sorted_headers(headers);
        self.next(
            &format!("{provider}:apikey"),
            &[key, base_url, proxy_url, prefix, &headers],
        )
    }
}

/// `GET /v0/management/gemini-api-key` (upstream's `GetGeminiKeys`).
async fn gemini_keys(State(state): State<ManagementState>) -> Response {
    let mut indexes = Indexes::new(&state);
    let config = state.config();
    let keys = config
        .gemini_api_key
        .iter()
        .map(|key| {
            let parts = [&*key.api_key, &key.base_url, &key.proxy_url, &key.prefix];
            let index = indexes.api_key("gemini", parts, &key.headers);
            config_json::gemini_key(key, &index)
        })
        .collect();
    list("gemini-api-key", keys)
}

/// `GET /v0/management/claude-api-key` (upstream's `GetClaudeKeys`).
async fn claude_keys(State(state): State<ManagementState>) -> Response {
    let mut indexes = Indexes::new(&state);
    let config = state.config();
    let keys = config
        .claude_api_key
        .iter()
        .map(|key| {
            let parts = [&*key.api_key, &key.base_url, &key.proxy_url, &key.prefix];
            let index = indexes.api_key("claude", parts, &key.headers);
            config_json::claude_key(key, &index)
        })
        .collect();
    list("claude-api-key", keys)
}

/// `GET /v0/management/codex-api-key` (upstream's `GetCodexKeys`).
async fn codex_keys(State(state): State<ManagementState>) -> Response {
    let mut indexes = Indexes::new(&state);
    let config = state.config();
    let keys = config
        .codex_api_key
        .iter()
        .map(|key| {
            let parts = [&*key.api_key, &key.base_url, &key.proxy_url, &key.prefix];
            let index = indexes.api_key("codex", parts, &key.headers);
            config_json::codex_key(key, &index)
        })
        .collect();
    list("codex-api-key", keys)
}

/// `GET /v0/management/vertex-api-key` (upstream's `GetVertexCompatKeys`).
async fn vertex_keys(State(state): State<ManagementState>) -> Response {
    let mut indexes = Indexes::new(&state);
    let config = state.config();
    let keys = config
        .vertex_api_key
        .iter()
        .map(|key| {
            let index = indexes.next(
                "vertex:apikey",
                &[&key.api_key, &key.base_url, &key.proxy_url],
            );
            config_json::vertex_key(key, &index)
        })
        .collect();
    list("vertex-api-key", keys)
}

/// `GET /v0/management/openai-compatibility` (upstream's
/// `GetOpenAICompat`). The credential of a provider without API keys is
/// shown on the provider, the others on each key.
async fn openai_compatibility(State(state): State<ManagementState>) -> Response {
    let mut indexes = Indexes::new(&state);
    let config = state.config();
    let entries = config
        .openai_compatibility
        .iter()
        .map(|entry| {
            let name = to_lower(entry.name.trim());
            let name = if name.is_empty() {
                "openai-compatibility"
            } else {
                &name
            };
            let kind = format!("openai-compatibility:{name}");
            let base_url = entry.base_url.trim();
            if entry.api_key_entries.is_empty() {
                let index = indexes.next(&kind, &[base_url]);
                return config_json::openai_compatibility_listed(entry, &index, &[]);
            }
            let key_indexes: Vec<String> = entry
                .api_key_entries
                .iter()
                .map(|key| indexes.next(&kind, &[&key.api_key, base_url, &key.proxy_url]))
                .collect();
            config_json::openai_compatibility_listed(entry, "", &key_indexes)
        })
        .collect();
    list("openai-compatibility", entries)
}

/// `{"<name>":[...]}`: `[]` when the list is empty.
fn list(name: &str, items: Vec<Json>) -> Response {
    json::response(StatusCode::OK, &Json::map([(name, Json::Array(items))]))
}

/// `GET /v0/management/oauth-excluded-models` (upstream's
/// `GetOAuthExcludedModels`). The config was cleaned up as it loaded.
async fn oauth_excluded_models(State(state): State<ManagementState>) -> Response {
    let models = config_json::excluded_models(&state.config().oauth_excluded_models);
    json::response(
        StatusCode::OK,
        &Json::map([("oauth-excluded-models", models)]),
    )
}

/// `GET /v0/management/oauth-model-alias` (upstream's `GetOAuthModelAlias`).
async fn oauth_model_alias(State(state): State<ManagementState>) -> Response {
    let aliases = config_json::model_aliases(&state.config().oauth_model_alias);
    json::response(StatusCode::OK, &Json::map([("oauth-model-alias", aliases)]))
}

/// `GET /v0/management/oauth-request-scoped-errors` (upstream's
/// `GetOAuthRequestScopedErrors`).
async fn oauth_request_scoped_errors(State(state): State<ManagementState>) -> Response {
    let rules = config_json::scoped_errors(&state.config().oauth_request_scoped_errors);
    json::response(
        StatusCode::OK,
        &Json::map([("oauth-request-scoped-errors", rules)]),
    )
}

/// `GET /v8/management/config` (upstream's `ConfigV8`): the whole config.
async fn config_v8(State(state): State<ManagementState>) -> Response {
    read_v8(&state, V8Read::Json(Some(Vec::new()))).await
}

/// `GET /v8/management/config/*path` (upstream's `ConfigV8`): the value at
/// the path, one mapping key per segment.
async fn config_v8_path(
    State(state): State<ManagementState>,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    // A path that isn't UTF-8 names no key.
    let parts = path.ok().map(|Path(path)| {
        let path = path.trim_matches('/');
        if path.is_empty() {
            Vec::new()
        } else {
            path.split('/').map(str::to_owned).collect()
        }
    });
    read_v8(&state, V8Read::Json(parts)).await
}

/// `GET /v8/management/config.yaml` (upstream's `ConfigV8`): the file.
async fn config_v8_yaml(State(state): State<ManagementState>) -> Response {
    read_v8(&state, V8Read::Yaml).await
}

/// What a v8 config read asks for.
enum V8Read {
    /// The value at a path, as JSON; `None` for a path that names no key.
    Json(Option<Vec<String>>),
    /// The file.
    Yaml,
}

/// Reads the config file in the v8 layout and answers `read`.
async fn read_v8(state: &ManagementState, read: V8Read) -> Response {
    let path = state.config_path().map(PathBuf::from);
    run_blocking(move || {
        let Some(data) = path.and_then(|path| std::fs::read(path).ok()) else {
            return json::error(StatusCode::INTERNAL_SERVER_ERROR, "read_failed");
        };
        let mut document = match V8Document::migrate(&data) {
            Ok(document) => document,
            Err(error) => return invalid_config(&error.to_string()),
        };
        let parts = match read {
            V8Read::Yaml => return yaml_response(data),
            V8Read::Json(None) => return json::error(StatusCode::NOT_FOUND, "not_found"),
            V8Read::Json(Some(parts)) => parts,
        };
        if let Some(key) = document.plain_management_key() {
            match management_key_hash(&key) {
                Ok(hash) => document.set_management_key_hash(&hash),
                Err(error) => {
                    return invalid_config(&format!(
                        "failed to hash remote management key: {error}"
                    ));
                }
            }
        }
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        document.project_aliases(&parts.join("."));
        document.redact_turn_secrets();
        let value = match document.value(&parts) {
            None => return json::error(StatusCode::NOT_FOUND, "not_found"),
            Some(Err(_)) => json::error(StatusCode::INTERNAL_SERVER_ERROR, "decode_failed"),
            Some(Ok(value)) => any_response(&value),
        };
        no_store(value)
    })
    .await
}

/// `{"error":"invalid_config","message":message}` with a 500.
fn invalid_config(message: &str) -> Response {
    json::response(
        StatusCode::INTERNAL_SERVER_ERROR,
        &Json::map([
            ("error", Json::Str("invalid_config".into())),
            ("message", Json::Str(message.to_owned())),
        ]),
    )
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The plain management key's bcrypt hash, as upstream's `hashSecret`
/// makes it (cost 10, `$2a$`), made once for each key.
fn management_key_hash(key: &str) -> Result<String, bcrypt::BcryptError> {
    static HASH: Mutex<Option<(String, String)>> = Mutex::new(None);
    let mut cached = HASH.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((plain, hash)) = cached.as_ref()
        && plain == key
    {
        return Ok(hash.clone());
    }
    let hash = bcrypt::hash_with_result(key, 10)?.format_for_version(bcrypt::Version::TwoA);
    *cached = Some((key.to_owned(), hash.clone()));
    Ok(hash)
}

/// A decoded YAML value as `c.JSON` writes it: when Go's encoder can't
/// write it (a mapping with a key that isn't a string, or an infinite or
/// NaN float), a 200 with no body.
fn any_response(value: &AnyValue) -> Response {
    let mut out = String::new();
    let body = write_any(&mut out, value).map_or_else(String::new, |()| out);
    let mut response = (StatusCode::OK, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

/// Writes `value` as Go's encoder writes what yaml.v3 decoded into `any`;
/// `None` where the encoder fails.
fn write_any(out: &mut String, value: &AnyValue) -> Option<()> {
    match value {
        AnyValue::Null => out.push_str("null"),
        AnyValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        AnyValue::Int(n) => out.push_str(&n.to_string()),
        AnyValue::Uint(n) => out.push_str(&n.to_string()),
        AnyValue::Float(f) => {
            if !f.is_finite() {
                return None;
            }
            out.push_str(&format_float(*f));
        }
        AnyValue::Str(s) => write_string(out, s.as_bytes()),
        AnyValue::Time(Some(text)) => write_string(out, text.as_bytes()),
        AnyValue::Seq(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_any(out, item)?;
            }
            out.push(']');
        }
        AnyValue::Map(entries) => {
            out.push('{');
            for (i, (key, item)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key.as_bytes());
                out.push(':');
                write_any(out, item)?;
            }
            out.push('}');
        }
        AnyValue::AnyMap | AnyValue::Time(None) => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ported from upstream's config_basic_weight_test.go
    /// (TestNormalizeRoutingStrategyWeightedRoundRobin), with the other
    /// strategies `normalizeRoutingStrategy` knows and one it doesn't, which
    /// is named as it is.
    #[test]
    fn strategies_are_named_as_upstream_names_them() {
        let cases = [
            ("weighted-round-robin", "weighted-round-robin"),
            ("weightedroundrobin", "weighted-round-robin"),
            ("wrr", "weighted-round-robin"),
            ("Weighted-Round-Robin", "weighted-round-robin"),
            ("", "round-robin"),
            (" RR ", "round-robin"),
            ("RoundRobin", "round-robin"),
            ("FF", "fill-first"),
            ("fillfirst", "fill-first"),
            ("  Sticky  ", "Sticky"),
        ];
        for (raw, want) in cases {
            assert_eq!(routing_strategy(raw), want, "{raw:?}");
        }
    }

    /// Not upstream's: values decoded from YAML are written as Go writes
    /// them, and those it can't write fail.
    #[test]
    fn decoded_values_are_written_as_go_writes_them() {
        let write = |value: &AnyValue| {
            let mut out = String::new();
            write_any(&mut out, value).map(|()| out)
        };
        let map = AnyValue::Map(
            [
                ("b".to_owned(), AnyValue::Float(1e21)),
                ("a".to_owned(), AnyValue::Uint(u64::MAX)),
                ("<".to_owned(), AnyValue::Str("&".into())),
            ]
            .into_iter()
            .collect(),
        );
        let seq = AnyValue::Seq(vec![
            AnyValue::Null,
            AnyValue::Bool(true),
            AnyValue::Int(-3),
            AnyValue::Float(0.5),
            map,
        ]);
        assert_eq!(
            write(&seq).as_deref(),
            Some(
                "[null,true,-3,0.5,{\"\\u003c\":\"\\u0026\",\"a\":18446744073709551615,\"b\":1e+21}]"
            )
        );
        assert_eq!(write(&AnyValue::Float(f64::INFINITY)), None);
        assert_eq!(write(&AnyValue::Float(f64::NAN)), None);
        assert_eq!(write(&AnyValue::Seq(vec![AnyValue::AnyMap])), None);
        let time = AnyValue::Time(Some("2002-12-14T00:00:00Z".into()));
        assert_eq!(write(&time).as_deref(), Some(r#""2002-12-14T00:00:00Z""#));
        assert_eq!(write(&AnyValue::Time(None)), None);
    }
}
