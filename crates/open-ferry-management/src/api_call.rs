// Ported from CLIProxyAPI internal/api/handlers/management/api_tools.go
// (APICall, apiCallRequest, apiCallResponse, firstNonEmptyString,
// tokenValueForAuth, resolveTokenForAuth, tokenValueFromMetadata)
// (v8.0.10, MIT), with what of Go's net/http client it relies on
// (client.go: Client.do, redirectBehavior, makeHeadersCopier,
// shouldCopyHeaderOnRedirect, isDomainOrSubdomain, refererForURL, send's
// basic auth; request.go: validMethod, Request.write's Host handling and
// request target, removeZone; response.go: fixPragmaCacheControl,
// isProtocolSwitchResponse; transfer.go: shouldSendContentLength,
// shouldClose, parseTransferEncoding, fixLength, parseContentLength,
// fixTrailer, readTransfer's body framing; transport.go: the gzip request
// and decoding, the body of a protocol switch; the HTTP/2 client's handling
// of Trailer; httpguts: headerValueContainsToken; textproto: TrimString)
// (go1.27, BSD-3-Clause).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! `POST /v0/management/api-call` (also
//! `/v8/management/requests/api-call`): sends an HTTP request for the
//! management client and answers with the response.
//!
//! The body gives the `method`, the `url`, the `header` map, the `data` to
//! send and, optionally, a credential by its `auth_index` (or `authIndex`,
//! or `AuthIndex`) and a `proxy_url`. `$TOKEN$` in a header value or in the
//! data is replaced by the credential's token: its access token, else its
//! API key, else another token its metadata holds. In data that is JSON, a
//! token holding quotes, backslashes or line breaks is escaped for a JSON
//! string. A `Host` header sets the host the request names. The proxy is
//! chosen as [`crate::proxy`] describes.
//!
//! The request carries exactly the caller's headers, and what HTTP needs
//! besides: a `Host`, a `Content-Length` for a body (and an empty one for
//! `POST`, `PUT` and `PATCH`), `Accept-Encoding: gzip` when the caller asks
//! for no encoding and no range (the response is then decompressed), and a
//! basic `Authorization` from a user name in the URL when the caller sends
//! none. A caller that sends no `User-Agent` gets `open-ferry/<version>`;
//! an empty one sends none. Redirects are followed as Go's client follows
//! them, at most ten, dropping credentials when the host changes to one
//! that isn't a subdomain. The call, redirects and reading included, has a
//! minute.
//!
//! The answer is 200 with the response's `status_code`, its `header` (each
//! name as Go writes it, with its values) and its `body` as text, whatever
//! the response's status; or 502 when the request fails or its body can't
//! be read. The body of a switch of protocols (a 101 with an `Upgrade` and
//! a `Connection: upgrade`) is all the connection sends until it closes;
//! that of a 2xx answer to `CONNECT` is read as any other's.
//!
//! Deviations from upstream:
//! - Credentials are never refreshed or minted here: an Antigravity, Meta
//!   or xAI credential's token is looked up as any other's. Upstream
//!   refreshes an Antigravity or xAI token about to expire, mints a Meta
//!   key from its `dca_token`, and answers `auth token refresh failed` when
//!   that fails; it also takes an xAI credential's token only from its
//!   `api_key` or access token, never its `id_token`.
//! - Without a `User-Agent` from the caller, upstream sends Go's
//!   `Go-http-client/1.1` (or `/2.0`); this port sends
//!   `open-ferry/<version>`.
//! - Without an `Accept` header from the caller, this port's HTTP client
//!   sends `Accept: */*`; upstream sends no `Accept`.
//! - A response body over 16 MiB, compressed or not, gives a 502 `failed to
//!   read response`; upstream reads any size.
//! - The header map is applied in the order of its names; upstream's order
//!   is random, which matters only when two names differ in case alone.
//! - A request with a `Host` header goes over HTTP/1.1, so that the header
//!   is sent as one; upstream may use HTTP/2 and send it as `:authority`.
//! - A `Host` header with characters outside ASCII gives a 502; upstream
//!   converts it to Punycode. A host outside ASCII is never treated as the
//!   same domain on a redirect, so credentials are dropped.
//! - Through a forwarding proxy (an HTTP or HTTPS proxy, for an `http`
//!   URL), the request line names the `Host` header's host, as upstream's
//!   does, but as the `url` crate reads it (lowercased, without a default
//!   port, an all-numeric host as an IPv4 address); a host it can't read
//!   gives a 502.
//! - A `CONNECT` request names a host and port alone: the URL's, or through
//!   a forwarding proxy the `Host` header's. Upstream names the URL's path
//!   when it has one, else the `Host` header's host or the URL's, and
//!   through a forwarding proxy the whole URL.
//! - The URL sent, a redirect's target and the `Referer` on a redirect are
//!   the `url` crate's reading of the URL, which may differ from Go's in
//!   normalization (a default port, an empty path); a URL Go accepts and
//!   the `url` crate doesn't, such as one with an IPv6 zone, gives a 502.
//! - `Expect: 100-continue` is sent but not waited on; the body goes at
//!   once.
//! - A 2xx answer to `CONNECT` with a chunked body gives a 502; upstream
//!   reads the body.
//! - An HTTP/1.0 response with a `Transfer-Encoding` gives a 502, as this
//!   port's HTTP client rejects it; upstream ignores the header.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http::header::{
    ACCEPT_ENCODING, AUTHORIZATION, CONTENT_LENGTH, HOST, HeaderMap, HeaderName, HeaderValue,
    USER_AGENT,
};
use http::{Method, StatusCode, Version};
use open_ferry_core::auth::Auth;
use open_ferry_translate::go::{json_string, json_valid};
use serde::de::MapAccess;
use serde_json::{Map, Value};
use tokio::io::AsyncReadExt as _;
use tokio::time::{Instant, timeout_at};

use crate::auth_files::run_blocking;
use crate::bind::{self, GoStruct, Nullable, StringMap, set_string};
use crate::go::{canonical_header_key, equal_fold, to_upper};
use crate::go_url::{self, GoUrl};
use crate::json::{self, Json};
use crate::proxy::{self, Route};
use crate::quota::auth_by_index;
use crate::state::ManagementState;

/// How long a call may take, redirects and reading the body included.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The largest response body read, before and after decompression.
pub(crate) const MAX_RESPONSE_BODY: usize = 16 << 20;

/// How many redirects are followed.
const MAX_REDIRECTS: usize = 10;

/// What a header value or the data names the credential's token by.
const TOKEN: &str = "$TOKEN$";

/// The user agent sent when the caller sends none.
const DEFAULT_USER_AGENT: &str = open_ferry_providers::codex::USER_AGENT;

/// The body of an `api-call` (upstream's `apiCallRequest`).
#[derive(Default)]
struct ApiCallRequest {
    auth_index_snake: Option<String>,
    auth_index_camel: Option<String>,
    auth_index_pascal: Option<String>,
    method: String,
    url: String,
    proxy_url: String,
    header: Option<BTreeMap<String, String>>,
    data: String,
}

impl GoStruct for ApiCallRequest {
    const FIELDS: &'static [&'static str] = &[
        "auth_index",
        "authIndex",
        "AuthIndex",
        "method",
        "url",
        "proxy_url",
        "header",
        "data",
    ];

    fn set<'de, A: MapAccess<'de>>(&mut self, index: usize, map: &mut A) -> Result<(), A::Error> {
        match index {
            0 => self.auth_index_snake = map.next_value::<Nullable>()?.0,
            1 => self.auth_index_camel = map.next_value::<Nullable>()?.0,
            2 => self.auth_index_pascal = map.next_value::<Nullable>()?.0,
            3 => set_string(&mut self.method, map)?,
            4 => set_string(&mut self.url, map)?,
            5 => set_string(&mut self.proxy_url, map)?,
            6 => match map.next_value::<StringMap>()?.0 {
                None => self.header = None,
                Some(entries) => self.header.get_or_insert_default().extend(entries),
            },
            _ => set_string(&mut self.data, map)?,
        }
        Ok(())
    }
}

/// `POST /v0/management/api-call` (upstream's `APICall`).
pub(crate) async fn api_call(State(state): State<ManagementState>, body: Body) -> Response {
    let body = match bind::read_body(body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(mut request) = bind::decode::<ApiCallRequest>(&body) else {
        return bad_request("invalid body");
    };

    let method = to_upper(request.method.trim());
    if method.is_empty() {
        return bad_request("missing method");
    }
    let url = request.url.trim().to_owned();
    if url.is_empty() {
        return bad_request("missing url");
    }
    let Some(mut target) =
        go_url::parse(url.as_bytes()).filter(|u| !u.scheme.is_empty() && !u.host.is_empty())
    else {
        return bad_request("invalid url");
    };
    // Go's NewRequest drops an empty port (removeEmptyPort).
    let last = |byte| target.host.iter().rposition(|&b| b == byte);
    if last(b':').is_some_and(|colon| Some(colon) > last(b']')) && target.host.ends_with(b":") {
        target.host.pop();
    }
    let request_proxy = request.proxy_url.trim().to_owned();
    if !request_proxy.is_empty() && proxy::parse(&request_proxy).is_none() {
        return bad_request("invalid proxy_url");
    }

    let auth_index = [
        &request.auth_index_snake,
        &request.auth_index_camel,
        &request.auth_index_pascal,
    ]
    .into_iter()
    .flatten()
    .map(|value| value.trim())
    .find(|value| !value.is_empty())
    .unwrap_or_default()
    .to_owned();
    let auth = auth_by_index(state.manager(), &auth_index);

    let mut tokens = Tokens {
        auth: auth.as_deref(),
        auth_index: &auth_index,
        token: None,
    };
    let mut header = request.header.take().unwrap_or_default();
    for value in header.values_mut() {
        if !value.contains(TOKEN) {
            continue;
        }
        match tokens.get() {
            Ok(token) => *value = value.replace(TOKEN, token),
            Err(message) => return bad_request(message),
        }
    }
    if request.data.contains(TOKEN) {
        let token = match tokens.get() {
            Ok(token) => token,
            Err(message) => return bad_request(message),
        };
        let replacement = if json_valid(request.data.as_bytes())
            && token.contains(['"', '\\', '\r', '\n', '\t'])
        {
            let quoted = json_string(token);
            quoted[1..quoted.len() - 1].to_owned()
        } else {
            token.to_owned()
        };
        request.data = request.data.replace(TOKEN, &replacement);
    }

    let Some(method) = valid_method(&method) else {
        return bad_request("failed to build request");
    };

    // Go's Header.Set for each entry, the Host header aside.
    let mut headers = BTreeMap::new();
    let mut host = String::new();
    for (key, value) in header {
        if equal_fold(&key, "host") {
            value.trim().clone_into(&mut host);
            continue;
        }
        headers.insert(canonical_header_key(&key), value);
    }

    let route = proxy::api_call_route(&state.config(), auth.as_deref(), &request_proxy);
    let first = Hop {
        method,
        url,
        go: target,
        host,
        headers,
        body: (!request.data.is_empty()).then(|| Bytes::from(request.data)),
    };
    match call(&state, &route, first).await {
        Ok(response) => json::response(StatusCode::OK, &response),
        Err(CallError::Request(reason)) => {
            tracing::debug!("management APICall request failed: {reason}");
            json::error(StatusCode::BAD_GATEWAY, "request failed")
        }
        Err(CallError::Read) => json::error(StatusCode::BAD_GATEWAY, "failed to read response"),
    }
}

fn bad_request(message: &str) -> Response {
    json::error(StatusCode::BAD_REQUEST, message)
}

/// The credential's token, looked up once, when first needed.
struct Tokens<'a> {
    auth: Option<&'a Auth>,
    auth_index: &'a str,
    token: Option<String>,
}

impl Tokens<'_> {
    fn get(&mut self) -> Result<&str, &'static str> {
        let auth = self.auth;
        let token = self
            .token
            .get_or_insert_with(|| auth.map(token_value_for_auth).unwrap_or_default());
        if token.is_empty() {
            if !self.auth_index.is_empty() && self.auth.is_none() {
                return Err("auth credential not found for auth_index");
            }
            return Err("auth token not found");
        }
        Ok(token)
    }
}

/// A credential's token: from its metadata, else its `api_key` or
/// `session_token` attribute (upstream's `tokenValueForAuth`).
pub(crate) fn token_value_for_auth(auth: &Auth) -> String {
    let token = token_value_from_metadata(&auth.metadata);
    if !token.is_empty() {
        return token;
    }
    ["api_key", "session_token"]
        .into_iter()
        .map(|key| auth.attribute(key).unwrap_or_default().trim())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// The token a credential's metadata holds, trimmed, or empty (upstream's
/// `tokenValueFromMetadata`).
fn token_value_from_metadata(metadata: &Map<String, Value>) -> String {
    let string = |map: &Map<String, Value>, key: &str| {
        map.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    if let Some(token) =
        string(metadata, "accessToken").or_else(|| string(metadata, "access_token"))
    {
        return token;
    }
    if let Some(Value::Object(token)) = metadata.get("token")
        && let Some(token) = string(token, "access_token").or_else(|| string(token, "accessToken"))
    {
        return token;
    }
    ["token", "id_token", "api_key", "session_token", "cookie"]
        .into_iter()
        .find_map(|key| string(metadata, key))
        .unwrap_or_default()
}

/// The method, if it is an HTTP token (Go's `validMethod`).
fn valid_method(method: &str) -> Option<Method> {
    let valid = !method.is_empty()
        && method.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        });
    if !valid {
        return None;
    }
    Method::from_bytes(method.as_bytes()).ok()
}

/// One request of a call: the first, or one following a redirect.
#[derive(Clone)]
struct Hop {
    method: Method,
    /// The URL as given or resolved.
    url: String,
    /// Go's reading of the URL.
    go: GoUrl,
    /// The `Host` the request names instead of the URL's, or empty (Go's
    /// `Request.Host`).
    host: String,
    /// The headers, by Go's canonical names.
    headers: BTreeMap<String, String>,
    body: Option<Bytes>,
}

/// Why a call failed.
enum CallError {
    /// No response: the reason, for the debug log. It never holds a URL or
    /// a header value.
    Request(String),
    /// The response's body couldn't be read.
    Read,
}

impl CallError {
    fn request(reason: &str) -> Self {
        Self::Request(reason.to_owned())
    }

    fn client(error: &reqwest::Error) -> Self {
        Self::Request(error_chain(error))
    }
}

/// A `reqwest` error and its sources, without the URL, which may hold a
/// secret.
fn error_chain(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// A response, read.
struct Received {
    status: u16,
    headers: BTreeMap<String, Vec<Vec<u8>>>,
    body: Vec<u8>,
}

/// Sends the call, following redirects, and reads the final response (Go's
/// `Client.Do` with a 60-second `Timeout`, then `io.ReadAll`).
async fn call(state: &ManagementState, route: &Route, first: Hop) -> Result<Json, CallError> {
    let deadline = Instant::now() + TIMEOUT;
    let initial = first.clone();
    let mut hop = first;
    let mut include_body = true;
    let mut strip_sensitive = false;
    let mut sent = 0;
    let (response, requested_gzip, method) = loop {
        let (response, requested_gzip) = timeout_at(deadline, send(state, route, &hop))
            .await
            .map_err(|_| CallError::request("timed out"))??;
        sent += 1;

        let status = response.status().as_u16();
        let (next_method, body_on_hop) = match status {
            301..=303 => {
                let method = if hop.method == Method::GET || hop.method == Method::HEAD {
                    hop.method.clone()
                } else {
                    Method::GET
                };
                (method, false)
            }
            307 | 308 => (hop.method.clone(), true),
            _ => break (response, requested_gzip, hop.method),
        };
        if !body_on_hop {
            include_body = false;
        }
        let location = response
            .headers()
            .get(http::header::LOCATION)
            .map_or(&b""[..], HeaderValue::as_bytes)
            .to_vec();
        if location.is_empty() {
            break (response, requested_gzip, hop.method);
        }
        let next = redirect(
            &initial,
            &hop,
            &location,
            next_method,
            include_body,
            &mut strip_sensitive,
        )?;
        if sent >= MAX_REDIRECTS {
            return Err(CallError::request("stopped after 10 redirects"));
        }
        drop(response);
        hop = next;
    };
    let received = timeout_at(deadline, read(response, requested_gzip, &method))
        .await
        .map_err(|_| CallError::Read)??;
    Ok(Json::Struct(vec![
        ("status_code", Json::Int(i64::from(received.status))),
        (
            "header",
            Json::Map(
                received
                    .headers
                    .into_iter()
                    .map(|(name, values)| {
                        let values = values.into_iter().map(Json::Bytes).collect();
                        (name, Json::Array(values))
                    })
                    .collect(),
            ),
        ),
        ("body", Json::Bytes(received.body)),
    ]))
}

/// The request following a redirect to `location` (the loop of Go's
/// `Client.do`).
fn redirect(
    initial: &Hop,
    previous: &Hop,
    location: &[u8],
    method: Method,
    include_body: bool,
    strip_sensitive: &mut bool,
) -> Result<Hop, CallError> {
    let invalid = || CallError::request("failed to parse Location header");
    let reference = go_url::parse(location).ok_or_else(invalid)?;
    let base = url::Url::parse(&previous.url).map_err(|_| invalid())?;
    let location = std::str::from_utf8(location).map_err(|_| invalid())?;
    let url = base.join(location).map_err(|_| invalid())?;
    // Go's URL.ResolveReference, as far as it is kept.
    let go = if reference.is_abs() || !reference.host.is_empty() || reference.user.is_some() {
        GoUrl {
            scheme: if reference.scheme.is_empty() {
                previous.go.scheme.clone()
            } else {
                reference.scheme.clone()
            },
            host: reference.host.clone(),
            user: reference.user.clone(),
        }
    } else {
        previous.go.clone()
    };
    // A Host the caller set survives a redirect to a relative location.
    let host = if !previous.host.is_empty()
        && previous.host.as_bytes() != previous.go.host
        && !reference.is_abs()
    {
        previous.host.clone()
    } else {
        String::new()
    };
    if !*strip_sensitive
        && initial.go.host != go.host
        && !should_copy_headers_on_redirect(&initial.go.host, &go.host)
    {
        *strip_sensitive = true;
    }
    let mut headers: BTreeMap<String, String> = initial
        .headers
        .iter()
        .filter(|(name, _)| {
            let sensitive = matches!(
                name.as_str(),
                "Authorization"
                    | "Www-Authenticate"
                    | "Cookie"
                    | "Cookie2"
                    | "Proxy-Authorization"
                    | "Proxy-Authenticate"
            );
            let body = matches!(
                name.as_str(),
                "Content-Encoding" | "Content-Language" | "Content-Location" | "Content-Type"
            );
            !(sensitive && *strip_sensitive) && !(body && !include_body)
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    // Go's refererForURL: no Referer from https to http; one the caller
    // set stays; otherwise the previous URL.
    if !(previous.go.scheme == "https" && go.scheme == "http") {
        let explicit = headers.get("Referer").cloned().unwrap_or_default();
        let referer = if explicit.is_empty() {
            let mut previous_url = base;
            let _ = previous_url.set_username("");
            let _ = previous_url.set_password(None);
            previous_url.to_string()
        } else {
            explicit
        };
        headers.insert("Referer".into(), referer);
    }
    Ok(Hop {
        method,
        url: url.to_string(),
        go,
        host,
        headers,
        body: if include_body {
            initial.body.clone()
        } else {
            None
        },
    })
}

/// Whether headers with credentials may follow a redirect from `initial`
/// to `dest`: the same host, or a subdomain of it, ports aside (Go's
/// `shouldCopyHeaderOnRedirect`).
fn should_copy_headers_on_redirect(initial: &[u8], dest: &[u8]) -> bool {
    let (initial, dest) = (hostname(initial), hostname(dest));
    // Go converts a host outside ASCII to Punycode; this port doesn't.
    if !initial.is_ascii() || !dest.is_ascii() {
        return false;
    }
    let (parent, sub) = (initial.to_ascii_lowercase(), dest.to_ascii_lowercase());
    if sub == parent {
        return true;
    }
    if sub.contains(&b':') || sub.contains(&b'%') {
        return false;
    }
    sub.len() > parent.len() && sub.ends_with(&parent) && sub[sub.len() - parent.len() - 1] == b'.'
}

/// A host without its port or brackets (Go's `URL.Hostname`).
fn hostname(host: &[u8]) -> &[u8] {
    let mut name = host;
    if let Some(colon) = host.iter().rposition(|&b| b == b':')
        && host[colon + 1..].iter().all(u8::is_ascii_digit)
    {
        name = &host[..colon];
    }
    if name.len() >= 2 && name.starts_with(b"[") && name.ends_with(b"]") {
        name = &name[1..name.len() - 1];
    }
    name
}

/// Whether every byte may appear in a `Host` header (`httpguts`'
/// `ValidHostHeader`).
fn valid_host_header(host: &[u8]) -> bool {
    host.iter().all(|&b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b'-'
                    | b'.'
                    | b':'
                    | b';'
                    | b'='
                    | b'['
                    | b']'
                    | b'_'
                    | b'~'
            )
    })
}

/// A bracketed host without its IPv6 zone (Go's `removeZone`).
fn remove_zone(host: &[u8]) -> Vec<u8> {
    if !host.starts_with(b"[") {
        return host.to_vec();
    }
    let Some(close) = host.iter().rposition(|&b| b == b']') else {
        return host.to_vec();
    };
    let Some(percent) = host[..close].iter().rposition(|&b| b == b'%') else {
        return host.to_vec();
    };
    let mut out = host[..percent].to_vec();
    out.extend_from_slice(&host[close..]);
    out
}

/// The headers Go writes itself, whatever the request's header map says
/// (`reqWriteExcludeHeader`; `Host` never reaches the map).
fn written_by_client(name: &str) -> bool {
    matches!(
        name,
        "User-Agent" | "Content-Length" | "Transfer-Encoding" | "Trailer"
    )
}

/// A header value as Go writes it: checked, then trimmed of spaces and
/// tabs (`httpguts.ValidHeaderFieldValue`, then `textproto.TrimString`).
fn header_value(value: &str) -> Result<HeaderValue, CallError> {
    HeaderValue::from_str(value)
        .and_then(|_| HeaderValue::from_str(value.trim_matches([' ', '\t'])))
        .map_err(|_| CallError::request("invalid header field value"))
}

/// Sends one request; returns the response and whether gzip was asked for
/// on the caller's behalf. The headers go in Go's order: `Host`,
/// `User-Agent`, `Content-Length`, the rest by name, then
/// `Accept-Encoding: gzip`.
async fn send(
    state: &ManagementState,
    route: &Route,
    hop: &Hop,
) -> Result<(reqwest::Response, bool), CallError> {
    // Go's transport checks every header in the map, written or not.
    let mut fields = BTreeMap::new();
    for (name, value) in &hop.headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| CallError::request("invalid header field name"))?;
        let header_value = header_value(value)?;
        if !written_by_client(name) {
            fields.insert(name.as_str(), (header_name, header_value));
        }
    }
    let empty = |name: &str| hop.headers.get(name).is_none_or(String::is_empty);
    if let Some((user, password)) = &hop.go.user
        && empty("Authorization")
    {
        let mut credentials = user.clone();
        credentials.push(b':');
        credentials.extend(password.as_deref().unwrap_or_default());
        let value = format!("Basic {}", STANDARD.encode(credentials));
        let value = HeaderValue::from_str(&value)
            .map_err(|_| CallError::request("invalid header field value"))?;
        fields.insert("Authorization", (AUTHORIZATION, value));
    }

    let mut headers = HeaderMap::new();
    let http1_only = !hop.host.is_empty();
    let forwards = route.forwards(&hop.go.scheme);
    // The host the request line names through a forwarding proxy, when the
    // caller set one.
    let mut target_host = None;
    if http1_only {
        let host = hop.host.as_bytes();
        if !host.is_ascii() {
            return Err(CallError::request("host outside ASCII"));
        }
        let host = if valid_host_header(host) {
            remove_zone(host)
        } else if forwards {
            return Err(CallError::request("http: invalid Host header"));
        } else {
            // Go sends an empty Host rather than an invalid one.
            Vec::new()
        };
        if forwards {
            target_host = Some(String::from_utf8_lossy(&host).into_owned());
        }
        let host = HeaderValue::from_bytes(&host)
            .map_err(|_| CallError::request("invalid header field value"))?;
        headers.insert(HOST, host);
    }
    match hop.headers.get("User-Agent") {
        None => {
            headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));
        }
        Some(agent) if agent.is_empty() => {}
        Some(agent) => {
            headers.insert(USER_AGENT, header_value(agent)?);
        }
    }
    match &hop.body {
        Some(body) => {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(body.len()));
        }
        None if matches!(hop.method, Method::POST | Method::PUT | Method::PATCH) => {
            headers.insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
        }
        None => {}
    }
    for (name, value) in fields.into_values() {
        headers.insert(name, value);
    }
    let requested_gzip = empty("Accept-Encoding") && empty("Range") && hop.method != Method::HEAD;
    if requested_gzip {
        headers.append(ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
    }

    let mut url = url::Url::parse(&hop.url).map_err(|_| CallError::request("invalid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(CallError::request("unsupported protocol scheme"));
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    // Through a forwarding proxy, Go's request line is the URL with the
    // Host's host in it; the proxy is reached whatever the URL names.
    if let Some(host) = target_host {
        let target = format!(
            "{}://{host}{}",
            url.scheme(),
            &url[url::Position::BeforePath..url::Position::AfterQuery]
        );
        url = url::Url::parse(&target)
            .map_err(|_| CallError::request("Host unreadable as a URL's host"))?;
    }

    let client = state
        .clients()
        .get(route, http1_only)
        .map_err(|error| CallError::client(&error.without_url()))?;
    let mut request = reqwest::Request::new(hop.method.clone(), url);
    *request.headers_mut() = headers;
    if let Some(body) = &hop.body {
        *request.body_mut() = Some(body.clone().into());
    }
    let response = client
        .execute(request)
        .await
        .map_err(|error| CallError::client(&error.without_url()))?;
    Ok((response, requested_gzip))
}

/// Reads the response: its headers as Go's client leaves them, and its
/// body, decompressed if gzip was asked for on the caller's behalf.
async fn read(
    mut response: reqwest::Response,
    requested_gzip: bool,
    method: &Method,
) -> Result<Received, CallError> {
    let status = response.status().as_u16();
    let version = response.version();
    let mut headers: BTreeMap<String, Vec<Vec<u8>>> = BTreeMap::new();
    for (name, value) in response.headers() {
        headers
            .entry(canonical_header_key(name.as_str()))
            .or_default()
            .push(value.as_bytes().to_vec());
    }
    let no_body = *method == Method::HEAD || matches!(status, 100..=199 | 204 | 304);
    let mut chunked = false;
    if version >= Version::HTTP_2 {
        // Go's HTTP/2 client keeps the trailer names out of the header.
        headers.remove("Trailer");
    } else {
        // Go's fixPragmaCacheControl.
        let no_cache = headers
            .get("Pragma")
            .and_then(|values| values.first())
            .is_some_and(|value| value == b"no-cache");
        if no_cache && !headers.contains_key("Cache-Control") {
            headers.insert("Cache-Control".into(), vec![b"no-cache".to_vec()]);
        }
        // Go's shouldClose: over HTTP/1.1 a `Connection` asking to close is
        // taken out.
        let close = headers
            .get("Connection")
            .is_some_and(|values| values.iter().any(|value| contains_token(value, b"close")));
        if version == Version::HTTP_11 && close {
            headers.remove("Connection");
        }
        // Go's parseTransferEncoding and fixLength.
        if let Some(encodings) = headers.remove("Transfer-Encoding")
            && version == Version::HTTP_11
        {
            if encodings.len() != 1 || !encodings[0].eq_ignore_ascii_case(b"chunked") {
                return Err(CallError::request("unsupported transfer encoding"));
            }
            chunked = true;
        }
        if let Some(lengths) = headers.get_mut("Content-Length")
            && lengths.len() > 1
        {
            let first = lengths[0].trim_ascii().to_vec();
            if lengths.iter().any(|length| length.trim_ascii() != first) {
                return Err(CallError::request("multiple Content-Length headers"));
            }
            *lengths = vec![first];
        }
        // Go's parseContentLength, which hyper skips where it reads no body.
        if let Some(lengths) = headers.get("Content-Length")
            && content_length(&lengths[0]).is_none()
        {
            return Err(CallError::request("bad Content-Length"));
        }
        if chunked && !no_body {
            headers.remove("Content-Length");
        }
        // Go's fixTrailer: a chunked response's trailer names leave the
        // header, and may not name the framing headers.
        if chunked && let Some(names) = headers.remove("Trailer") {
            let framing = names
                .iter()
                .flat_map(|value| value.split(|&b| b == b','))
                .filter_map(|name| std::str::from_utf8(name).ok())
                .map(|name| canonical_header_key(trim_string(name)))
                .any(|name| {
                    matches!(
                        name.as_str(),
                        "Transfer-Encoding" | "Trailer" | "Content-Length"
                    )
                });
            if framing {
                return Err(CallError::request("bad trailer key"));
            }
        }
    }
    let declared_empty = headers
        .get("Content-Length")
        .and_then(|lengths| lengths.first())
        .and_then(|length| content_length(length))
        == Some(0);
    let has_body = !no_body && (chunked || !declared_empty);
    let gunzip = requested_gzip
        && has_body
        && headers
            .get("Content-Encoding")
            .and_then(|values| values.first())
            .is_some_and(|value| value.eq_ignore_ascii_case(b"gzip"));
    if gunzip {
        headers.remove("Content-Encoding");
        headers.remove("Content-Length");
    }

    let mut body = match body_reader(status, method, &headers, chunked)? {
        BodyReader::Hyper => {
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| CallError::Read)? {
                if body.len() + chunk.len() > MAX_RESPONSE_BODY {
                    return Err(CallError::Read);
                }
                body.extend_from_slice(&chunk);
            }
            body
        }
        BodyReader::ToClose => read_handed_over(response, None).await?,
        BodyReader::Length(length) => read_handed_over(response, Some(length)).await?,
    };
    if gunzip && !body.is_empty() {
        body = run_blocking(move || gunzip_capped(&body)).await?;
    }
    Ok(Received {
        status,
        headers,
        body,
    })
}

/// A `Content-Length` as Go reads it (`parseContentLength`): digits, spaces
/// aside, for a number below 2^63.
fn content_length(value: &[u8]) -> Option<u64> {
    let digits = value.trim_ascii();
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits)
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|&length| length < 1 << 63)
}

/// Who reads a response's body, and how far.
enum BodyReader {
    /// Hyper, as the response frames it (or there is none).
    Hyper,
    /// This module, from the connection hyper hands over, to its close.
    ToClose,
    /// This module, from the connection hyper hands over, this many bytes.
    Length(u64),
}

/// Who reads the body. Hyper hands the connection over rather than read a
/// body after any 101 and after a 2xx answer to `CONNECT`. Go reads the
/// body of a switch of protocols (a 101 with an `Upgrade` and a
/// `Connection: upgrade`) to the connection's close, finds none in another
/// 101, and reads a 2xx answer to `CONNECT` as any response.
fn body_reader(
    status: u16,
    method: &Method,
    headers: &BTreeMap<String, Vec<Vec<u8>>>,
    chunked: bool,
) -> Result<BodyReader, CallError> {
    if status == 101 {
        let upgrade = headers
            .get("Upgrade")
            .and_then(|values| values.first())
            .is_some_and(|value| !value.is_empty());
        let connection = headers
            .get("Connection")
            .is_some_and(|values| values.iter().any(|value| contains_token(value, b"upgrade")));
        return Ok(if upgrade && connection {
            BodyReader::ToClose
        } else {
            BodyReader::Hyper
        });
    }
    if *method != Method::CONNECT || !(200..300).contains(&status) {
        return Ok(BodyReader::Hyper);
    }
    if chunked {
        return Err(CallError::request("chunked body after CONNECT"));
    }
    let length = headers
        .get("Content-Length")
        .and_then(|lengths| lengths.first())
        .and_then(|length| content_length(length));
    Ok(length.map_or(BodyReader::ToClose, BodyReader::Length))
}

/// Reads the body from the connection hyper hands over: `length` bytes, or
/// all until it closes, at most [`MAX_RESPONSE_BODY`].
async fn read_handed_over(
    response: reqwest::Response,
    length: Option<u64>,
) -> Result<Vec<u8>, CallError> {
    let connection = response.upgrade().await.map_err(|_| CallError::Read)?;
    let cap = MAX_RESPONSE_BODY as u64 + 1;
    let mut body = Vec::new();
    connection
        .take(length.map_or(cap, |length| length.min(cap)))
        .read_to_end(&mut body)
        .await
        .map_err(|_| CallError::Read)?;
    if body.len() > MAX_RESPONSE_BODY || length.is_some_and(|length| body.len() as u64 != length) {
        return Err(CallError::Read);
    }
    Ok(body)
}

/// Whether `value`, a comma-separated list, holds `token`, ASCII case
/// aside (`httpguts.headerValueContainsToken`).
fn contains_token(value: &[u8], token: &[u8]) -> bool {
    value
        .split(|&b| b == b',')
        .any(|element| trim_ows(element).eq_ignore_ascii_case(token))
}

/// `bytes` without leading and trailing spaces and tabs
/// (`httpguts.trimOWS`).
fn trim_ows(bytes: &[u8]) -> &[u8] {
    let ows = |b: &u8| *b == b' ' || *b == b'\t';
    let start = bytes.iter().position(|b| !ows(b)).unwrap_or(bytes.len());
    let end = bytes.iter().rposition(|b| !ows(b)).map_or(start, |i| i + 1);
    &bytes[start..end]
}

/// `s` without leading and trailing ASCII white space
/// (`textproto.TrimString`).
fn trim_string(s: &str) -> &str {
    s.trim_matches([' ', '\t', '\n', '\r'])
}

/// Decompresses gzip members, refusing more than [`MAX_RESPONSE_BODY`]
/// bytes of output.
fn gunzip_capped(compressed: &[u8]) -> Result<Vec<u8>, CallError> {
    let mut out = Vec::new();
    let limit = u64::try_from(MAX_RESPONSE_BODY).unwrap_or(u64::MAX) + 1;
    flate2::read::MultiGzDecoder::new(compressed)
        .take(limit)
        .read_to_end(&mut out)
        .map_err(|_| CallError::Read)?;
    if out.len() > MAX_RESPONSE_BODY {
        return Err(CallError::Read);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_looked_up_in_upstream_order() {
        let metadata = |value: Value| value.as_object().cloned().unwrap();
        let lookup = |value: Value| token_value_from_metadata(&metadata(value));
        assert_eq!(
            lookup(serde_json::json!({"access_token": " a ", "accessToken": "b"})),
            "b"
        );
        assert_eq!(
            lookup(serde_json::json!({"accessToken": " ", "access_token": "a"})),
            "a"
        );
        assert_eq!(
            lookup(serde_json::json!({"token": {"accessToken": "t2"}})),
            "t2"
        );
        assert_eq!(
            lookup(serde_json::json!({"token": {"access_token": "t1", "accessToken": "t2"}})),
            "t1"
        );
        assert_eq!(
            lookup(serde_json::json!({"token": {}, "id_token": "i"})),
            "i"
        );
        assert_eq!(lookup(serde_json::json!({"token": 5, "cookie": "c"})), "c");
        assert_eq!(
            lookup(serde_json::json!({"session_token": "s", "api_key": "k"})),
            "k"
        );
        assert_eq!(lookup(serde_json::json!({})), "");

        let auth = Auth {
            attributes: [("session_token".into(), " s ".into())].into(),
            ..Auth::default()
        };
        assert_eq!(token_value_for_auth(&auth), "s");
    }

    #[test]
    fn methods_must_be_tokens() {
        assert_eq!(valid_method("GET"), Some(Method::GET));
        assert!(valid_method("M-SEARCH").is_some());
        assert_eq!(valid_method("GE T"), None);
        assert_eq!(valid_method("G\u{c9}T"), None);
        assert_eq!(valid_method(""), None);
    }

    #[test]
    fn redirects_keep_credentials_within_a_domain() {
        let copy = |a: &str, b: &str| should_copy_headers_on_redirect(a.as_bytes(), b.as_bytes());
        assert!(copy("example.com", "example.com:8443"));
        assert!(copy("Example.com", "api.EXAMPLE.com"));
        assert!(!copy("api.example.com", "example.com"));
        assert!(!copy("example.com", "badexample.com"));
        assert!(copy("[::1]:80", "[::1]"));
        assert!(!copy("example.com", "[::1%25.example.com]"));
        assert!(!copy("ex\u{e4}mple.com", "ex\u{e4}mple.com"));
    }

    #[test]
    fn hosts_are_cleaned_as_go_cleans_them() {
        assert!(valid_host_header(b"[fe80::1%en0]:8080"));
        assert!(!valid_host_header(b"a b"));
        assert!(!valid_host_header(b"a/b"));
        assert_eq!(remove_zone(b"[fe80::1%en0]:80"), b"[fe80::1]:80");
        assert_eq!(remove_zone(b"host%x"), b"host%x");
        assert_eq!(hostname(b"[::1]:80"), b"::1");
        assert_eq!(hostname(b"host:"), b"host");
        assert_eq!(hostname(b"host:x"), b"host:x");
    }

    #[test]
    fn decompression_is_capped() {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&vec![0; MAX_RESPONSE_BODY + 1]).unwrap();
        let big = encoder.finish().unwrap();
        assert!(matches!(gunzip_capped(&big), Err(CallError::Read)));

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(b"ok").unwrap();
        let mut two = encoder.finish().unwrap();
        two.extend(two.clone());
        assert_eq!(gunzip_capped(&two).ok(), Some(b"okok".to_vec()));
        assert!(gunzip_capped(b"not gzip").is_err());
    }

    /// Go's decode of these bodies into upstream's `apiCallRequest`: `None`
    /// where it fails, else the fields it sets (go1.27.1 building for go
    /// 1.26.0, without `GOEXPERIMENT=jsonv2`).
    #[test]
    fn bodies_decode_as_go_decodes_them() {
        let summary = |request: ApiCallRequest| {
            let mut parts = Vec::new();
            let indexes = [
                ("auth_index", &request.auth_index_snake),
                ("authIndex", &request.auth_index_camel),
                ("AuthIndex", &request.auth_index_pascal),
            ];
            for (name, value) in indexes {
                if let Some(value) = value {
                    parts.push(format!("{name}={value}"));
                }
            }
            let strings = [
                ("method", &request.method),
                ("url", &request.url),
                ("proxy_url", &request.proxy_url),
            ];
            for (name, value) in strings {
                if !value.is_empty() {
                    parts.push(format!("{name}={value}"));
                }
            }
            if let Some(header) = &request.header {
                let entries: Vec<_> = header.iter().map(|(k, v)| format!("{k}:{v}")).collect();
                parts.push(format!("header={{{}}}", entries.join(",")));
            }
            if !request.data.is_empty() {
                parts.push(format!("data={}", request.data));
            }
            parts.join(" ")
        };
        let cases: &[(&[u8], Option<&str>)] = &[
            (b"", None),
            (b" ", None),
            (b"null", Some("")),
            (b"{}", Some("")),
            (b"[]", None),
            (b"1", None),
            (b"\"x\"", None),
            (b"{\"method\":\"GET\"}", Some("method=GET")),
            (b"{\"method\":1}", None),
            (b"{\"METHOD\":\"a\"}", Some("method=a")),
            (b"{\"Method\":\"a\",\"method\":\"b\"}", Some("method=b")),
            (b"{\"method\":\"b\",\"Method\":\"a\"}", Some("method=a")),
            (b"{\"method\":null}", Some("")),
            (b"{\"auth_index\":null}", Some("")),
            (b"{\"auth_index\":\"x\"}", Some("auth_index=x")),
            (b"{\"authindex\":\"x\"}", Some("authIndex=x")),
            (b"{\"AUTHINDEX\":\"y\"}", Some("authIndex=y")),
            (
                b"{\"auth_index\":\"a\",\"authIndex\":\"b\",\"AuthIndex\":\"c\"}",
                Some("auth_index=a authIndex=b AuthIndex=c"),
            ),
            (
                b"{\"header\":{\"a\":\"1\"},\"header\":{\"b\":\"2\"}}",
                Some("header={a:1,b:2}"),
            ),
            (b"{\"header\":null}", Some("")),
            (b"{\"header\":{\"a\":1}}", None),
            (b"{\"header\":[]}", None),
            (b"{\"header\":{\"a\":null}}", Some("header={a:}")),
            (b"{\"data\":\"x\",\"data\":null}", Some("data=x")),
            (b"{\"url\":\"\x5cu0041\"}", Some("url=A")),
            (b"{\"x\":1}{", Some("")),
            (b"{\"method\":\"a\"} trailing", Some("method=a")),
            (b"{\"method\":\"a\"", None),
            (b"{\"method\":\"\xff\"}", Some("method=\u{fffd}")),
            (b"{\"auth_\xc4\xb1ndex\":\"i\"}", Some("")),
            (b"{\"auth_\xc4\xb0ndex\":\"j\"}", Some("")),
            (b"{\"AUTH_INDEX\":\" idx \"}", Some("auth_index= idx ")),
            (b"{\"auth_index\":1}", None),
            (b"{\"auth_index\":\"a\",\"auth_index\":null}", Some("")),
            (
                b"{\"AuthIndex\":\"p\",\"authindex\":\"q\"}",
                Some("authIndex=q AuthIndex=p"),
            ),
            (b"{\"x\":1e400}", Some("")),
            (
                b"{\"method\":\"GET\",\"x\":[1,{\"y\":null}]}",
                Some("method=GET"),
            ),
            (b"\xef\xbb\xbf{}", None),
            (b"{\"method\":\"a\"}\n\n", Some("method=a")),
            (b" {\"method\":\"a\"} x", Some("method=a")),
            (b"{} {", Some("")),
            (b"nul", None),
            (b"null x", Some("")),
            (b"{\"method\":\"a\",}", None),
            (b"{\"method\":\"a\" \"url\":\"b\"}", None),
            (
                b"{\"header\":{\"a\":\"1\",\"A\":\"2\"}}",
                Some("header={A:2,a:1}"),
            ),
            (b"{\"header\":{\"\":\"x\"}}", Some("header={:x}")),
            (b"{\"method\":\"a\",\"method\":\"b\"}", Some("method=b")),
            (b"{\"method\":true}", None),
            (
                b"{\"header\":{\"a\":\"1\"},\"header\":null,\"header\":{\"c\":\"3\"}}",
                Some("header={c:3}"),
            ),
            (b"{\"header\":\"x\"}", None),
            (b"{\"data\":{\"a\":1}}", None),
            (b"{\"Data\":\"D\",\"DATA\":\"E\"}", Some("data=E")),
            (
                b"{\"proxy_url\":\"p\",\"PROXY_URL\":\"q\",\"proxyurl\":\"r\"}",
                Some("proxy_url=q"),
            ),
            (
                b"{\"auth_index\":\"s\",\"AUTH_INDEX\":\"t\"}",
                Some("auth_index=t"),
            ),
            (
                b"{\"authIndex\":null,\"AUTHindex\":\"u\"}",
                Some("authIndex=u"),
            ),
            (b"{\"m\xc4\xb0thod\":\"v\"}", Some("")),
            (b"{\"URL\":\"w\"}", Some("url=w")),
            (b"{\"ur\xc4\xb1\":\"x\"}", Some("")),
            (b"{\"method\":\"\x5cu00e9\"}", Some("method=\u{e9}")),
            (b"{\"method\":\"a\x5cu0000b\"}", Some("method=a\0b")),
            (b"{\"method\":-0}", None),
            (b"[{}]", None),
            (b"{\"auth_index\":[\"a\"]}", None),
            (
                b"{\"header\":{\"a\":\"1\",\"a\":\"2\"}}",
                Some("header={a:2}"),
            ),
        ];
        for &(body, want) in cases {
            let got = bind::decode::<ApiCallRequest>(body).map(summary);
            assert_eq!(got.as_deref(), want, "{}", String::from_utf8_lossy(body));
        }
        // Go reads an unpaired surrogate escape as U+FFFD; serde_json fails
        // (a deviation the bind module notes).
        let body = [&br#"{"url":""#[..], b"\\", br#"ud800"}"#].concat();
        assert!(bind::decode::<ApiCallRequest>(&body).is_none());
    }
}
