//! `GET /client-setup`: what the app needs to write client configs, from
//! the config and the model registry.
//!
//! Each model says when it came out and whether it is a chat model, so the
//! app can suggest a current model from the registry's own data rather than
//! a list of its own that would go stale.

use std::collections::BTreeSet;
use std::net::IpAddr;

use axum::extract::State;
use axum::response::Response;
use open_ferry_core::auth::compat::OPENAI_COMPATIBILITY;
use open_ferry_core::codex_models::is_image_or_video_model;
use open_ferry_core::config::Config;
use open_ferry_core::exec::Format;
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::ModelRegistry;
use open_ferry_core::registry::registration::OPENAI_IMAGE_MODEL_TYPE;
use open_ferry_server::entry_providers;
use serde::Serialize;

use super::ok;
use crate::{DashboardState, Listener};

/// A root URL the server can be reached at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct BaseUrl {
    url: String,
    source: &'static str,
}

/// One of the proxy's entry points, and the models a call to it can use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Route {
    id: &'static str,
    protocol: &'static str,
    method: &'static str,
    path: &'static str,
    base_path: &'static str,
    models: Vec<String>,
}

/// A model on any route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Model {
    id: String,
    display_name: String,
    owned_by: String,
    providers: Vec<String>,
    created: Option<i64>,
    chat: bool,
    context_length: Option<u64>,
    max_output_tokens: Option<u64>,
}

/// The answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ClientSetup {
    base_urls: Vec<BaseUrl>,
    tls: bool,
    safe_mode: bool,
    separate_management: bool,
    routes: Vec<Route>,
    models: Vec<Model>,
}

/// An entry point: its ID, protocol, the format it takes, its path and the
/// base path an SDK for it takes.
struct EntryPoint {
    id: &'static str,
    protocol: &'static str,
    format: Format,
    path: &'static str,
    base_path: &'static str,
}

/// The proxy's entry points, in ID order.
const ENTRY_POINTS: [EntryPoint; 5] = [
    EntryPoint {
        id: "claude-messages",
        protocol: "claude",
        format: Format::CLAUDE,
        path: "/v1/messages",
        base_path: "",
    },
    EntryPoint {
        id: "codex-responses",
        protocol: "codex",
        format: Format::OPENAI_RESPONSE,
        path: "/backend-api/codex/responses",
        base_path: "/backend-api/codex",
    },
    EntryPoint {
        id: "gemini-generate-content",
        protocol: "gemini",
        format: Format::GEMINI,
        path: "/v1beta/models/{model}:generateContent",
        base_path: "",
    },
    EntryPoint {
        id: "openai-chat-completions",
        protocol: "openai",
        format: Format::OPENAI,
        path: "/v1/chat/completions",
        base_path: "/v1",
    },
    EntryPoint {
        id: "openai-responses",
        protocol: "openai-responses",
        format: Format::OPENAI_RESPONSE,
        path: "/v1/responses",
        base_path: "/v1",
    },
];

/// `GET /client-setup`.
pub(super) async fn client_setup(State(state): State<DashboardState>) -> Response {
    let config = state.management.config();
    let separate = state.listener == Listener::Separate;
    ok(&setup(&config, state.management.registry(), separate))
}

/// The client setup for `config` and the models in `registry`, served on
/// the management address's own listener if `separate`.
pub(crate) fn setup(config: &Config, registry: &ModelRegistry, separate: bool) -> ClientSetup {
    let infos = registry.available_model_infos();
    let mut on_any = BTreeSet::new();
    let mut models = Vec::new();
    let mut routes: Vec<Route> = ENTRY_POINTS
        .iter()
        .map(|entry| Route {
            id: entry.id,
            protocol: entry.protocol,
            method: "POST",
            path: entry.path,
            base_path: entry.base_path,
            models: Vec::new(),
        })
        .collect();
    for info in &infos {
        let providers = registry.providers_for_model(&info.id);
        let mut reachable = false;
        for (route, entry) in routes.iter_mut().zip(&ENTRY_POINTS) {
            if !entry_providers(&entry.format, &info.id, providers.clone()).is_empty() {
                route.models.push(info.id.clone());
                reachable = true;
            }
        }
        if reachable && on_any.insert(info.id.clone()) {
            models.push(model(info, providers));
        }
    }
    ClientSetup {
        base_urls: base_urls(config, separate),
        tls: config.tls.enable,
        safe_mode: config.has_example_api_keys(),
        separate_management: separate,
        routes,
        models,
    }
}

/// How the client setup describes `info`, served by `providers`.
fn model(info: &ModelInfo, providers: Vec<String>) -> Model {
    let known = |value: u64| (value > 0).then_some(value);
    Model {
        id: info.id.clone(),
        display_name: if info.display_name.is_empty() {
            info.id.clone()
        } else {
            info.display_name.clone()
        },
        owned_by: info.owned_by.clone(),
        providers,
        created: released(info),
        chat: is_chat(info),
        context_length: known(info.max_context_length)
            .or_else(|| known(info.context_length))
            .or_else(|| known(info.input_token_limit)),
        max_output_tokens: known(info.max_completion_tokens)
            .or_else(|| known(info.output_token_limit)),
    }
}

/// When `info` came out, in Unix seconds, as the model catalog has it. A
/// model defined in the config has the time the config was loaded as its
/// `created` instead (upstream's), which says nothing of the model and
/// changes from one load to the next, so it has none here; nor has one whose
/// `created` is unknown.
fn released(info: &ModelInfo) -> Option<i64> {
    let configured = info.user_defined
        || info.model_type == OPENAI_COMPATIBILITY
        || info.model_type == OPENAI_IMAGE_MODEL_TYPE;
    (info.created > 0 && !configured).then_some(info.created)
}

/// Whether `info` is a chat model, as far as its details say: not an image
/// or video model, by the list Codex clients hide them by or as configured,
/// and, where its details list them, answering in text alone and through
/// `generateContent`. That leaves out Gemini's image models, Imagen and
/// the embedding models; a model whose details list neither counts as one.
fn is_chat(info: &ModelInfo) -> bool {
    let text_only = info
        .supported_output_modalities
        .iter()
        .all(|modality| modality.eq_ignore_ascii_case("text"));
    let generates = info.supported_generation_methods.is_empty()
        || info
            .supported_generation_methods
            .iter()
            .any(|method| method == "generateContent");
    text_only
        && generates
        && info.model_type != OPENAI_IMAGE_MODEL_TYPE
        && !is_image_or_video_model(&info.id)
}

/// The roots the server can be reached at: from where it listens, then the
/// configured base URL, unless `separate`: then `management.base-url` is
/// the management address's, which doesn't serve the proxy.
pub(crate) fn base_urls(config: &Config, separate: bool) -> Vec<BaseUrl> {
    let scheme = if config.tls.enable { "https" } else { "http" };
    let host = config.host.trim();
    let hosts: Vec<String> = match host {
        "" | "::" | "[::]" => vec!["127.0.0.1".into(), "[::1]".into(), "localhost".into()],
        "0.0.0.0" => vec!["127.0.0.1".into(), "localhost".into()],
        host => match host.parse::<IpAddr>() {
            Ok(IpAddr::V6(address)) => vec![format!("[{address}]")],
            _ => vec![host.to_owned()],
        },
    };
    let mut urls: Vec<BaseUrl> = hosts
        .into_iter()
        .map(|host| BaseUrl {
            url: format!("{scheme}://{host}:{}", config.port),
            source: "listen",
        })
        .collect();
    if !separate
        && let Some(url) = configured_base_url(&config.remote_management.base_url)
        && !urls.iter().any(|known| known.url == url)
    {
        urls.push(BaseUrl {
            url,
            source: "config",
        });
    }
    urls
}

/// `management.base-url` without credentials, query, fragment or a
/// trailing `/`, if it is an HTTP or HTTPS URL.
fn configured_base_url(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut url = url::Url::parse(text).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    let url = url.to_string();
    Some(url.trim_end_matches('/').to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(host: &str, port: i64, tls: bool, base_url: &str) -> Config {
        let mut config = Config::default();
        config.host = host.to_owned();
        config.port = port;
        config.tls.enable = tls;
        config.remote_management.base_url = base_url.to_owned();
        config
    }

    fn urls(config: &Config) -> Vec<(String, &'static str)> {
        base_urls(config, false)
            .into_iter()
            .map(|url| (url.url, url.source))
            .collect()
    }

    /// Not upstream's: a server listening on every address offers the
    /// loopback ones, and a configured base URL loses its credentials,
    /// query and fragment.
    #[test]
    fn base_urls_come_from_the_config() {
        assert_eq!(
            urls(&config("", 8317, false, "")),
            vec![
                ("http://127.0.0.1:8317".to_owned(), "listen"),
                ("http://[::1]:8317".to_owned(), "listen"),
                ("http://localhost:8317".to_owned(), "listen"),
            ]
        );
        assert_eq!(
            urls(&config("0.0.0.0", 9000, true, "")),
            vec![
                ("https://127.0.0.1:9000".to_owned(), "listen"),
                ("https://localhost:9000".to_owned(), "listen"),
            ]
        );
        assert_eq!(
            urls(&config(
                "::1",
                8317,
                false,
                "https://user:pass@proxy.example.com/base/?key=x#y"
            )),
            vec![
                ("http://[::1]:8317".to_owned(), "listen"),
                ("https://proxy.example.com/base".to_owned(), "config"),
            ]
        );
        assert_eq!(
            urls(&config("proxy.lan", 8317, false, "ftp://proxy.example.com")),
            vec![("http://proxy.lan:8317".to_owned(), "listen")]
        );
        assert_eq!(
            urls(&config("127.0.0.1", 8317, false, "http://127.0.0.1:8317/")),
            vec![("http://127.0.0.1:8317".to_owned(), "listen")]
        );
    }

    /// Not upstream's: served on the management address's own listener,
    /// `management.base-url` isn't offered, as it is the management
    /// address's.
    #[test]
    fn a_separate_listener_leaves_out_the_base_url() {
        let config = config("::1", 8317, false, "http://127.0.0.1:8318");
        let urls: Vec<(String, &str)> = base_urls(&config, true)
            .into_iter()
            .map(|url| (url.url, url.source))
            .collect();
        assert_eq!(urls, vec![("http://[::1]:8317".to_owned(), "listen")]);
    }
}
