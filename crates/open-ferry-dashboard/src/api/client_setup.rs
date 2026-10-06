//! `GET /client-setup`: what the app needs to write client configs, from
//! the config and the model registry.

use std::collections::BTreeSet;
use std::net::IpAddr;

use axum::extract::State;
use axum::response::Response;
use open_ferry_core::config::Config;
use open_ferry_core::exec::Format;
use open_ferry_core::models::ModelInfo;
use open_ferry_core::registry::ModelRegistry;
use open_ferry_server::entry_providers;
use serde::Serialize;

use super::ok;
use crate::DashboardState;

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
    context_length: Option<u64>,
    max_output_tokens: Option<u64>,
}

/// The answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ClientSetup {
    base_urls: Vec<BaseUrl>,
    tls: bool,
    safe_mode: bool,
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
    ok(&setup(&config, state.management.registry()))
}

/// The client setup for `config` and the models in `registry`.
pub(crate) fn setup(config: &Config, registry: &ModelRegistry) -> ClientSetup {
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
        base_urls: base_urls(config),
        tls: config.tls.enable,
        safe_mode: config.has_example_api_keys(),
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
        context_length: known(info.max_context_length)
            .or_else(|| known(info.context_length))
            .or_else(|| known(info.input_token_limit)),
        max_output_tokens: known(info.max_completion_tokens)
            .or_else(|| known(info.output_token_limit)),
    }
}

/// The roots the server can be reached at: from where it listens, then the
/// configured base URL.
pub(crate) fn base_urls(config: &Config) -> Vec<BaseUrl> {
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
    if let Some(url) = configured_base_url(&config.remote_management.base_url)
        && !urls.iter().any(|known| known.url == url)
    {
        urls.push(BaseUrl {
            url,
            source: "config",
        });
    }
    urls
}

/// `remote-management.base-url` without credentials, query, fragment or a
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
        base_urls(config)
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
}
