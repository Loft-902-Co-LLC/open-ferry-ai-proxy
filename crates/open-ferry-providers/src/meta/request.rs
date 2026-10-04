// Ported from CLIProxyAPI internal/runtime/executor/meta_executor.go
// (metaCreds), meta_executor_execute.go (prepareResponsesRequest,
// applyMetaAPIHeaders) and meta_test.go (TestMetaExecutor_MetaCredsResolution,
// TestMetaExecutor_PreservesPreviousResponseID, and the
// ClientIdHeader_Issue6117 tests, inverted) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! What a Meta call is made of: the credential's token and base URL, the
//! request's headers, and its body translated to the Responses format Meta
//! takes (Codex's, without what only Codex reads).
//!
//! The body is the client's, translated, then adjusted by the thinking
//! suffix and the payload rules, with these fields dropped: `generate`,
//! `prompt_cache_retention`, `safety_identifier`, `stream_options` and
//! `client_metadata`. `previous_response_id` is kept, unlike for Codex. A
//! custom `apply_patch` tool is declared as a function (see
//! [`crate::apply_patch_responses`]), `instructions` is filled in, and
//! reasoning items, `web_search` tools and a Codex client's tool parameter
//! types are made fit for Meta.
//!
//! Deviations from upstream:
//! - The request carries no client identity of Meta's: no `X-Client-Id`
//!   (a credential's custom header of that name is dropped), and no
//!   `muse-build` user agent. The user agent is the client's, as for the
//!   other providers, else `open-ferry/<version>`; a credential's
//!   `header:User-Agent` doesn't set it (see [`crate::custom_headers`]).
//! - A credential with no token is refused with one message. Upstream's
//!   differs for a `meta-api-key` entry, and mentions the DCA tokens, which
//!   aren't used here. A token that starts with `dca:` isn't one.
//! - The credential's token is never one of Meta's OAuth storage; see
//!   [`super`].
//! - A request in a client's compatibility model isn't translated through
//!   the compatibility translator (`is_compat`), which belongs to Codex's
//!   `codex-api-key` entries.

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderValue};
use open_ferry_core::auth::Auth;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{ErrorKind, ExecError, Format, Options, Request};
use open_ferry_core::models::ModelCatalog;
use open_ferry_translate::codex_client::{header_value, tool_integers};
use open_ferry_translate::registry::Registry;
use serde_json::Value;

use super::tools::sanitize_web_search_tools;
use crate::apply_patch_responses;
use crate::codex::client::USER_AGENT;
use crate::codex::compat;
use crate::codex::reasoning::sanitize_reasoning;
use crate::codex::request::{
    base_model, ensure_header, normalize_instructions, parse_object, response_format,
    set_bool_if_different, set_string_if_different,
};
use crate::codex::terminal::StatusError;
use crate::codex::thinking;
use crate::custom_headers;
use crate::json::{self, delete};
use crate::payload;
use crate::thinking::Route;

/// Meta's API (`metaauth.DefaultAPIBaseURL`).
pub(super) const DEFAULT_BASE_URL: &str = "https://api.meta.ai/v1";

/// What the payload rules call Meta, as the executor and as the protocol it
/// takes.
const META: Format = Format::from_static("meta");

/// The message of the 401 for a credential with no token.
pub(super) const MISSING_TOKEN_MESSAGE: &str = "meta executor: missing API key or access token";

/// The fields of the body Meta doesn't take.
const DROPPED_FIELDS: [&str; 5] = [
    "generate",
    "prompt_cache_retention",
    "safety_identifier",
    "stream_options",
    "client_metadata",
];

/// Where a call goes and the token it carries.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Creds {
    /// The API's base URL.
    pub(super) base_url: String,
    /// The API key or access token; empty if the credential has none.
    pub(super) token: String,
}

/// The base URL and token of `auth` (`metaCreds`).
///
/// The `base_url` attribute sets the URL, then the metadata's `base_url` or
/// `api_base_url` if the URL is still the default. The token is the
/// `api_key` attribute, else the `access_token` attribute, else the same two
/// in the metadata. A value that is blank, or starts with `dca:` (a token
/// that isn't an API key), is passed over.
pub(super) fn creds(auth: &Auth) -> Creds {
    let attribute = |key: &str| auth.attribute(key).unwrap_or_default().trim();
    let metadata = |key: &str| auth.metadata_str(key).unwrap_or_default().trim();
    let usable = |value: &str| !value.is_empty() && !value.starts_with("dca:");

    let mut base_url = DEFAULT_BASE_URL;
    let mut token = "";
    if !attribute("base_url").is_empty() {
        base_url = attribute("base_url");
    }
    if usable(attribute("api_key")) {
        token = attribute("api_key");
    } else if usable(attribute("access_token")) {
        token = attribute("access_token");
    }
    if base_url == DEFAULT_BASE_URL {
        if !metadata("base_url").is_empty() {
            base_url = metadata("base_url");
        } else if !metadata("api_base_url").is_empty() {
            base_url = metadata("api_base_url");
        }
    }
    if token.is_empty() {
        if usable(metadata("api_key")) {
            token = metadata("api_key");
        } else if usable(metadata("access_token")) {
            token = metadata("access_token");
        }
    }
    Creds {
        base_url: base_url.to_owned(),
        token: token.to_owned(),
    }
}

/// The URL of the Responses endpoint at `base_url`, less one trailing `/`.
pub(super) fn endpoint(base_url: &str) -> String {
    format!(
        "{}/responses",
        base_url.strip_suffix('/').unwrap_or(base_url)
    )
}

/// The error for a credential with no token (`ensureAuth`).
pub(super) fn missing_token() -> ExecError {
    StatusError::new(401, MISSING_TOKEN_MESSAGE).into()
}

/// The headers of a request to Meta, which always asks for an event stream
/// (`applyMetaAPIHeaders`): the JSON content type, the token as a bearer
/// (left out when empty), the user agent, what the stream needs, and the
/// credential's custom headers, which can't set `X-Client-Id`.
pub(super) fn build_headers(
    auth: &Auth,
    token: &str,
    client: &HeaderMap,
) -> Result<HeaderMap, ExecError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if !token.is_empty() {
        let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            ExecError::new(
                ErrorKind::Upstream,
                "meta executor: the credential's token isn't a valid header value",
            )
        })?;
        value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, value);
    }
    if !ensure_header(&mut headers, client, header::USER_AGENT) {
        headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    }
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    custom_headers::apply(&mut headers, &auth.attributes, client, "meta");
    if headers.remove("x-client-id").is_some() {
        tracing::warn!("meta: a custom X-Client-Id header would identify the client; not sent");
    }
    Ok(headers)
}

/// A request ready to send, with what its response is read with.
pub(super) struct Prepared {
    /// What to send.
    pub(super) body: Value,
    /// The bridge for `apply_patch` calls, which the response goes through.
    pub(super) apply_patch: apply_patch_responses::State,
    /// The format the client gets.
    pub(super) response_format: Format,
    /// The client's request, for response translators.
    pub(super) original: Value,
    /// The client's request as it came, for the Claude input estimate.
    pub(super) original_bytes: Bytes,
}

/// Translates the client's payload to the Responses format Meta takes
/// (`TranslateRequestWithAPIKeyModelCompatibility`, without compatibility
/// models): a Codex client's tools get integer parameter types and a
/// Responses request is readied as for any provider but Codex.
fn translate(
    config: Option<&Config>,
    options: &Options,
    base: &str,
    mut payload: Value,
    stream: bool,
) -> Value {
    compat::before_translation(config, options, &Format::CODEX, &mut payload);
    Registry::global().translate_request(
        &options.source_format,
        &Format::CODEX,
        base,
        payload,
        stream,
    )
}

/// Translates and adjusts the payload of a call, which asks for a stream if
/// `stream` is set (`prepareResponsesRequest`).
pub(super) fn prepare(
    config: Option<&Config>,
    models: Option<&dyn ModelCatalog>,
    request: &Request,
    options: &Options,
    stream: bool,
) -> Result<Prepared, ExecError> {
    let base = base_model(&request.model);
    let original_bytes = if options.original_request.is_empty() {
        request.payload.clone()
    } else {
        options.original_request.clone()
    };
    let original = parse_object(&original_bytes);
    let original_translated = translate(config, options, base, original.clone(), stream);
    let mut body = translate(
        config,
        options,
        base,
        parse_object(&request.payload),
        stream,
    );

    let to = Format::CODEX;
    let route = Route {
        model: &request.model,
        from: options.source_format.as_str(),
        to: to.as_str(),
        provider: "meta",
    };
    thinking::apply_request(
        &mut body,
        route,
        &json::Body::parse(&request.payload),
        &json::Body::parse(&options.original_request),
        models,
    )?;

    let protocol = META;
    let target = payload::Target {
        executor: "meta",
        protocol: &protocol,
        model: base,
        root: "",
        stream,
        tracked: &[],
        translate: Some(&|_| original_translated.clone()),
    };
    payload::apply(config, &target, request, options, &mut body);
    set_string_if_different(&mut body, "model", base);
    set_bool_if_different(&mut body, "stream", stream);
    for field in DROPPED_FIELDS {
        delete(&mut body, field);
    }

    let apply_patch =
        apply_patch_responses::State::new(&options.source_format, &original, &original_translated);
    apply_patch_responses::normalize_request(&mut body, Some(&original))
        .map_err(|error| ExecError::new(ErrorKind::Upstream, error.to_string()))?;
    normalize_instructions(&mut body, false);
    sanitize_reasoning(&mut body, false);
    sanitize_web_search_tools(&mut body);
    let user_agent = header_value(
        options
            .headers
            .get_all(header::USER_AGENT)
            .iter()
            .map(HeaderValue::as_bytes),
    );
    tool_integers::normalize(&mut body, &user_agent);

    Ok(Prepared {
        body,
        apply_patch,
        response_format: response_format(options),
        original,
        original_bytes,
    })
}

#[cfg(test)]
mod tests {
    use open_ferry_core::auth::Auth;
    use serde_json::json;

    use super::*;

    fn auth(attributes: &[(&str, &str)], metadata: &[(&str, &str)]) -> Auth {
        let mut auth = Auth::default();
        for (key, value) in attributes {
            auth.attributes
                .insert((*key).to_owned(), (*value).to_owned());
        }
        for (key, value) in metadata {
            auth.metadata.insert((*key).to_owned(), json!(value));
        }
        auth
    }

    fn creds_of(auth: &Auth) -> (String, String) {
        let creds = creds(auth);
        (creds.base_url, creds.token)
    }

    // TestMetaExecutor_MetaCredsResolution: the attributes win, the
    // metadata gives the token when they have none, and a credential with
    // nothing has no token and Meta's own URL.
    #[test]
    fn creds_resolve_from_attributes_then_metadata() {
        let (base, token) = creds_of(&auth(
            &[
                ("api_key", "attr-key"),
                ("base_url", "https://custom.meta.com/v1"),
            ],
            &[],
        ));
        assert_eq!(
            (base.as_str(), token.as_str()),
            ("https://custom.meta.com/v1", "attr-key")
        );

        let (base, token) = creds_of(&auth(&[], &[("access_token", "meta-oauth-token")]));
        assert_eq!(
            (base.as_str(), token.as_str()),
            (DEFAULT_BASE_URL, "meta-oauth-token")
        );

        let (base, token) = creds_of(&Auth::default());
        assert_eq!((base.as_str(), token.as_str()), (DEFAULT_BASE_URL, ""));
    }

    // Not upstream's: the order of the sources, and what is passed over.
    #[test]
    fn creds_skip_blank_and_dca_values() {
        for (attributes, metadata, want) in [
            // The API key first, then the access token.
            (
                vec![("api_key", " key "), ("access_token", "access")],
                vec![],
                "key",
            ),
            (vec![("access_token", " access ")], vec![], "access"),
            // A blank or DCA key falls through to the next.
            (
                vec![("api_key", "  "), ("access_token", "access")],
                vec![],
                "access",
            ),
            (
                vec![("api_key", "dca:minted"), ("access_token", "access")],
                vec![],
                "access",
            ),
            (vec![("api_key", "dca:minted")], vec![], ""),
            (vec![("access_token", "dca:minted")], vec![], ""),
            // The metadata is read only without an attribute token.
            (
                vec![("api_key", "attr")],
                vec![("api_key", "meta"), ("access_token", "oauth")],
                "attr",
            ),
            (
                vec![("api_key", "dca:minted")],
                vec![("api_key", " meta "), ("access_token", "oauth")],
                "meta",
            ),
            (
                vec![],
                vec![("api_key", "dca:x"), ("access_token", "oauth")],
                "oauth",
            ),
            (
                vec![],
                vec![("api_key", "dca:x"), ("access_token", "dca:y")],
                "",
            ),
        ] {
            let (_, token) = creds_of(&auth(&attributes, &metadata));
            assert_eq!(token, want, "{attributes:?} {metadata:?}");
        }

        // A DCA token key is never a token, in any form.
        let (_, token) = creds_of(&auth(&[("dca_token", "dca:minted")], &[("dca_token", "x")]));
        assert_eq!(token, "");
    }

    #[test]
    fn base_url_resolves_from_attributes_then_metadata() {
        for (attributes, metadata, want) in [
            (vec![], vec![], DEFAULT_BASE_URL),
            (vec![("base_url", "  ")], vec![], DEFAULT_BASE_URL),
            (
                vec![("base_url", " https://a.test/v1 ")],
                vec![],
                "https://a.test/v1",
            ),
            (
                vec![],
                vec![("base_url", " https://b.test/v1 ")],
                "https://b.test/v1",
            ),
            (
                vec![],
                vec![("api_base_url", "https://c.test/v1")],
                "https://c.test/v1",
            ),
            (
                vec![],
                vec![
                    ("base_url", "https://b.test/v1"),
                    ("api_base_url", "https://c.test/v1"),
                ],
                "https://b.test/v1",
            ),
            // An attribute wins over the metadata.
            (
                vec![("base_url", "https://a.test/v1")],
                vec![("base_url", "https://b.test/v1")],
                "https://a.test/v1",
            ),
            // The default itself, spelled out, still gives way to the metadata.
            (
                vec![("base_url", DEFAULT_BASE_URL)],
                vec![("base_url", "https://b.test/v1")],
                "https://b.test/v1",
            ),
        ] {
            let (base, _) = creds_of(&auth(&attributes, &metadata));
            assert_eq!(base, want, "{attributes:?} {metadata:?}");
        }
    }

    #[test]
    fn endpoint_trims_one_trailing_slash() {
        assert_eq!(endpoint("https://a.test/v1"), "https://a.test/v1/responses");
        assert_eq!(
            endpoint("https://a.test/v1/"),
            "https://a.test/v1/responses"
        );
        assert_eq!(
            endpoint("https://a.test/v1//"),
            "https://a.test/v1//responses"
        );
    }

    // TestMetaExecutor_ClientIdHeader_Issue6117, inverted: Meta's client ID
    // and its `muse-build` user agent are never sent.
    #[test]
    fn headers_carry_no_client_identity() {
        let auth = auth(&[("api_key", "meta-token")], &[]);
        let headers = build_headers(&auth, "meta-token", &HeaderMap::new()).unwrap();
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["authorization"], "Bearer meta-token");
        assert!(headers["authorization"].is_sensitive());
        assert_eq!(headers["accept"], "text/event-stream");
        assert_eq!(headers["cache-control"], "no-cache");
        assert_eq!(headers["user-agent"], USER_AGENT);
        assert!(USER_AGENT.starts_with("open-ferry/"));
        assert_eq!(headers.len(), 5, "{headers:?}");
        assert!(!headers.contains_key("x-client-id"));
    }

    #[test]
    fn headers_pass_the_clients_user_agent_on() {
        let auth = auth(&[], &[]);
        let mut client = HeaderMap::new();
        client.insert(
            header::USER_AGENT,
            HeaderValue::from_static("  curl/8.7.1 "),
        );
        client.insert("x-client-id", HeaderValue::from_static("client-chosen"));
        let headers = build_headers(&auth, "meta-token", &client).unwrap();
        assert_eq!(headers["user-agent"], "curl/8.7.1");
        assert!(!headers.contains_key("x-client-id"));

        // Blank is as good as none.
        let mut client = HeaderMap::new();
        client.insert(header::USER_AGENT, HeaderValue::from_static("  "));
        let headers = build_headers(&auth, "meta-token", &client).unwrap();
        assert_eq!(headers["user-agent"], USER_AGENT);
    }

    // Upstream sets the credential's custom headers last, `X-Client-Id`
    // included, and its user agent too, through `header:User-Agent`.
    #[test]
    fn custom_headers_cannot_set_the_client_identity() {
        let auth = auth(
            &[
                ("header:X-Client-Id", "tbh:tui"),
                ("header:x-client-id", "$X-Client-Id"),
                ("header:User-Agent", "muse-build/1.3.0 (interactive)"),
                ("header:X-Team", "blue"),
            ],
            &[],
        );
        let mut client = HeaderMap::new();
        client.insert("x-client-id", HeaderValue::from_static("client-chosen"));
        let headers = build_headers(&auth, "meta-token", &client).unwrap();
        assert!(!headers.contains_key("x-client-id"), "{headers:?}");
        assert_eq!(headers["user-agent"], USER_AGENT);
        assert_eq!(headers["x-team"], "blue");
        for value in headers.values() {
            let value = value.to_str().unwrap_or_default();
            assert!(
                !value.contains("muse-") && !value.contains("tbh:"),
                "{value}"
            );
        }
    }

    #[test]
    fn an_empty_token_sends_no_authorization() {
        let headers = build_headers(&Auth::default(), "", &HeaderMap::new()).unwrap();
        assert!(!headers.contains_key("authorization"));
        let error = build_headers(&Auth::default(), "bad\ntoken", &HeaderMap::new()).unwrap_err();
        assert!(error.message.contains("isn't a valid header value"));
        assert!(!error.message.contains("bad"));
    }

    // TestMetaExecutor_PreservesPreviousResponseID, and what else the
    // body is cleaned of.
    #[test]
    fn prepare_drops_only_what_meta_does_not_take() {
        let payload = json!({
            "model": "muse-spark-1.3",
            "input": "hi",
            "previous_response_id": "resp_prev",
            "generate": false,
            "prompt_cache_retention": "24h",
            "safety_identifier": "user-1",
            "stream_options": {"include_usage": true},
            "client_metadata": {"a": "b"},
        });
        let request = Request {
            model: "muse-spark-1.3(high)".to_owned(),
            payload: Bytes::from(payload.to_string()),
            ..Request::default()
        };
        let mut options = Options::new(Format::OPENAI_RESPONSE);
        options.original_request = request.payload.clone();
        let prepared = prepare(None, None, &request, &options, true).unwrap();
        let body = &prepared.body;
        assert_eq!(body["previous_response_id"], "resp_prev");
        assert_eq!(body["model"], "muse-spark-1.3");
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "");
        for field in DROPPED_FIELDS {
            assert!(body.get(field).is_none(), "{field} was kept");
        }
        // The Responses format Codex takes has none of these either.
        assert!(body.get("messages").is_none());
        assert!(body["input"].is_array());
        assert_eq!(prepared.response_format, Format::OPENAI_RESPONSE);
        assert!(!prepared.apply_patch.active());
    }

    #[test]
    fn prepare_asks_for_a_stream_only_when_told_to() {
        let request = Request {
            model: "muse-spark-1.3".to_owned(),
            payload: Bytes::from(r#"{"model":"muse-spark-1.3","input":"hi","stream":true}"#),
            ..Request::default()
        };
        let options = Options::new(Format::OPENAI_RESPONSE);
        let off = prepare(None, None, &request, &options, false).unwrap();
        assert_eq!(off.body["stream"], false);
        let on = prepare(None, None, &request, &options, true).unwrap();
        assert_eq!(on.body["stream"], true);
    }
}
