// Ported from CLIProxyAPI sdk/api/handlers/handlers_routing.go
// (preferExecutionProvider, adjustExecutionProvidersForEntryProtocol,
// supportsNativeInteractionsEntryProtocol and excludeExecutionProvider)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Which of a model's providers a call may use, given the format the client
//! sent: the Gemini Interactions provider serves only the formats it can
//! take natively.
//!
//! An Interactions request tries `gemini-interactions` first. Chat
//! Completions, Responses, Messages and Gemini requests keep it where the
//! catalog put it. Every other format (Codex, for one) never goes to it.
//!
//! Deviations from upstream: none.

use open_ferry_core::exec::{Format, ProviderId};
use open_ferry_translate::go;

/// The Gemini Interactions provider (upstream's `GeminiInteractions`).
pub(crate) const GEMINI_INTERACTIONS: &str = "gemini-interactions";

/// `providers`, adjusted for a client that sent `format` (upstream's
/// `adjustExecutionProvidersForEntryProtocol`).
pub(crate) fn adjust_execution_providers(
    format: &Format,
    providers: Vec<ProviderId>,
) -> Vec<ProviderId> {
    if *format == Format::INTERACTIONS {
        return prefer_execution_provider(providers, GEMINI_INTERACTIONS);
    }
    if supports_native_interactions(format) {
        return providers;
    }
    exclude_execution_provider(providers, GEMINI_INTERACTIONS)
}

/// Whether the Interactions provider takes requests in `format` (upstream's
/// `supportsNativeInteractionsEntryProtocol`).
fn supports_native_interactions(format: &Format) -> bool {
    [
        Format::INTERACTIONS,
        Format::OPENAI,
        Format::OPENAI_RESPONSE,
        Format::CLAUDE,
        Format::GEMINI,
    ]
    .contains(format)
}

/// Upstream's comparison of provider names: trimmed and lower-cased.
fn key(provider: &str) -> String {
    go::to_lower(provider.trim())
}

/// `providers` with the first that is `preferred` moved to the front
/// (upstream's `preferExecutionProvider`).
fn prefer_execution_provider(mut providers: Vec<ProviderId>, preferred: &str) -> Vec<ProviderId> {
    let preferred = key(preferred);
    if preferred.is_empty() || providers.len() < 2 {
        return providers;
    }
    match providers.iter().position(|p| key(p) == preferred) {
        Some(index) if index > 0 => {
            let moved = providers.remove(index);
            providers.insert(0, moved);
            providers
        }
        _ => providers,
    }
}

/// `providers` without the first that is `excluded` (upstream's
/// `excludeExecutionProvider`).
fn exclude_execution_provider(mut providers: Vec<ProviderId>, excluded: &str) -> Vec<ProviderId> {
    let excluded = key(excluded);
    if excluded.is_empty() {
        return providers;
    }
    if let Some(index) = providers.iter().position(|p| key(p) == excluded) {
        providers.remove(index);
    }
    providers
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::config::ServerConfig;
    use crate::router;
    use crate::testing::{FakeCatalog, FakeDispatcher, Outcome, state};

    fn names(names: &[&str]) -> Vec<ProviderId> {
        names.iter().map(|&name| name.to_owned()).collect()
    }

    // Ports TestPreferExecutionProviderMovesPreferredFirst.
    #[test]
    fn prefer_moves_the_preferred_provider_first() {
        let providers = prefer_execution_provider(
            names(&["gemini", "gemini-interactions", "claude"]),
            GEMINI_INTERACTIONS,
        );
        assert_eq!(providers, ["gemini-interactions", "gemini", "claude"]);
    }

    // Not upstream's: the edges of preferExecutionProvider and
    // excludeExecutionProvider.
    #[test]
    fn prefer_and_exclude_match_trimmed_and_ignoring_case() {
        assert_eq!(
            prefer_execution_provider(
                names(&["a", " Gemini-Interactions "]),
                "gemini-interactions"
            ),
            [" Gemini-Interactions ", "a"]
        );
        assert_eq!(
            prefer_execution_provider(names(&["gemini-interactions", "a"]), GEMINI_INTERACTIONS),
            ["gemini-interactions", "a"]
        );
        assert_eq!(
            prefer_execution_provider(names(&["a", "b"]), GEMINI_INTERACTIONS),
            ["a", "b"]
        );
        assert_eq!(
            prefer_execution_provider(names(&["a", "b"]), " "),
            ["a", "b"]
        );
        assert_eq!(
            exclude_execution_provider(
                names(&["GEMINI-INTERACTIONS", "codex", "gemini-interactions"]),
                GEMINI_INTERACTIONS
            ),
            ["codex", "gemini-interactions"]
        );
        assert_eq!(exclude_execution_provider(names(&["a"]), ""), ["a"]);
        assert!(exclude_execution_provider(Vec::new(), GEMINI_INTERACTIONS).is_empty());
    }

    // Ports TestAdjustExecutionProvidersExcludesInteractionsProviderForUnsupportedEntry.
    #[test]
    fn adjust_excludes_the_interactions_provider_for_unsupported_entries() {
        let providers =
            adjust_execution_providers(&Format::CODEX, names(&["gemini-interactions", "codex"]));
        assert_eq!(providers, ["codex"]);
    }

    // Ports TestAdjustExecutionProvidersKeepsInteractionsProviderForSupportedNativeInteractionsEntries.
    #[test]
    fn adjust_keeps_the_interactions_provider_for_native_entries() {
        for format in [
            Format::OPENAI,
            Format::OPENAI_RESPONSE,
            Format::CLAUDE,
            Format::GEMINI,
        ] {
            let providers = adjust_execution_providers(&format, names(&["gemini-interactions"]));
            assert_eq!(providers, ["gemini-interactions"], "{format}");
        }
    }

    // Not upstream's: each entry format, with the Interactions provider
    // second among three.
    #[test]
    fn adjust_for_each_entry_format() {
        let catalog = || names(&["gemini", "gemini-interactions", "claude"]);
        let kept = ["gemini", "gemini-interactions", "claude"];
        assert_eq!(
            adjust_execution_providers(&Format::INTERACTIONS, catalog()),
            ["gemini-interactions", "gemini", "claude"]
        );
        for format in [
            Format::OPENAI,
            Format::OPENAI_RESPONSE,
            Format::CLAUDE,
            Format::GEMINI,
        ] {
            assert_eq!(
                adjust_execution_providers(&format, catalog()),
                kept,
                "{format}"
            );
        }
        for format in [
            Format::CODEX,
            Format::ANTIGRAVITY,
            Format::from_static("gemini-cli"),
            Format::from_static(""),
        ] {
            assert_eq!(
                adjust_execution_providers(&format, catalog()),
                ["gemini", "claude"],
                "{format}"
            );
        }
    }

    /// A POST of `body` to `uri` with the test key.
    fn post(uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", "Bearer sk-test")
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    // Ports TestExecuteModelStreamKeepsInteractionsProviderForOpenAIEntry
    // through the Chat Completions route.
    #[tokio::test]
    async fn an_openai_stream_keeps_the_interactions_provider() {
        let model = "gemini-3.1-flash-lite";
        let catalog = FakeCatalog::new().serve(model, &["gemini-interactions"]);
        let dispatcher = FakeDispatcher::new([Outcome::chunks(&[
            r#"{"id":"chunk_1","object":"chat.completion.chunk","choices":[]}"#,
        ])]);
        let config = ServerConfig {
            api_keys: vec!["sk-test".into()],
            ..ServerConfig::default()
        };
        let app = router(state(config, catalog, &dispatcher));
        let body = r#"{"model":"gemini-3.1-flash-lite","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
        let response = app
            .oneshot(post("/v1/chat/completions", body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.into_body().collect().await.unwrap();

        let calls = dispatcher.calls();
        let [call] = calls.as_slice() else {
            panic!("calls = {calls:?}");
        };
        assert_eq!(call.providers, ["gemini-interactions"]);
        assert_eq!(call.request.model, model);
        assert_eq!(call.options.source_format, Format::OPENAI);
    }

    // Not upstream's: each route's call gets the providers adjusted for its
    // entry format.
    #[tokio::test]
    async fn each_route_adjusts_for_its_entry_format() {
        let catalog = FakeCatalog::new().serve("m", &["gemini", "gemini-interactions"]);
        let routes = [
            ("/v1/chat/completions", r#"{"model":"m","messages":[]}"#),
            ("/v1/completions", r#"{"model":"m","prompt":"hi"}"#),
            ("/v1/responses", r#"{"model":"m","input":"hi"}"#),
            (
                "/v1/messages",
                r#"{"model":"m","max_tokens":1,"messages":[]}"#,
            ),
            (
                "/v1/messages/count_tokens",
                r#"{"model":"m","messages":[]}"#,
            ),
            ("/v1beta/models/m:generateContent", r#"{"contents":[]}"#),
            ("/v1beta/models/m:countTokens", r#"{"contents":[]}"#),
            ("/v1beta/interactions", r#"{"model":"m","input":"hi"}"#),
        ];
        let dispatcher = FakeDispatcher::new(routes.iter().map(|_| Outcome::reply("{}")));
        let config = ServerConfig {
            api_keys: vec!["sk-test".into()],
            ..ServerConfig::default()
        };
        let app = router(state(config, catalog, &dispatcher));
        for (uri, body) in routes {
            let response = app.clone().oneshot(post(uri, body)).await.unwrap();
            response.into_body().collect().await.unwrap();
        }

        let calls = dispatcher.calls();
        let got: Vec<(String, Vec<ProviderId>)> = calls
            .iter()
            .map(|call| {
                (
                    call.options.source_format.to_string(),
                    call.providers.clone(),
                )
            })
            .collect();
        let kept = || vec!["gemini".to_owned(), "gemini-interactions".to_owned()];
        let want: Vec<(String, Vec<ProviderId>)> = vec![
            ("openai".into(), kept()),
            ("openai".into(), kept()),
            ("openai-response".into(), kept()),
            ("claude".into(), kept()),
            ("claude".into(), kept()),
            ("gemini".into(), kept()),
            ("gemini".into(), kept()),
            (
                "interactions".into(),
                vec!["gemini-interactions".to_owned(), "gemini".to_owned()],
            ),
        ];
        assert_eq!(got, want);
    }

    // Not upstream's: `entry_providers`, which the dashboard's client setup
    // lists each route's models with, adjusts as a call is routed and
    // leaves image-only models out, and speech-only ones outside the speech
    // endpoints' format.
    #[test]
    fn entry_providers_route_as_calls_do() {
        let both = || names(&["gemini-interactions", "gemini"]);
        assert_eq!(
            crate::entry_providers(&Format::OPENAI_RESPONSE, "gemini-3-pro", both()),
            both()
        );
        assert_eq!(
            crate::entry_providers(&Format::CODEX, "gemini-3-pro", both()),
            names(&["gemini"])
        );
        assert_eq!(
            crate::entry_providers(&Format::OPENAI, "gpt-image-2", names(&["codex"])),
            names(&[])
        );
        assert_eq!(
            crate::entry_providers(&Format::OPENAI, "grok-tts", names(&["xai"])),
            names(&[])
        );
        assert_eq!(
            crate::entry_providers(&Format::OPENAI_SPEECH, "grok-tts", names(&["xai"])),
            names(&["xai"])
        );
    }
}
