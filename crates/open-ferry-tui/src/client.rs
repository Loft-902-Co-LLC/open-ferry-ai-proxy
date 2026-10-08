// Ported from CLIProxyAPI internal/tui/client.go (NewClient,
// NewClientWithBaseURL, BaseURL, SetSecretKey, doRequest, get, put, patch,
// getJSON, postJSON, GetConfig, GetAuthFiles, DeleteAuthFile,
// ToggleAuthFile, PatchAuthFileFields, RefreshAuthFile, GetLogs,
// GetAPIKeys, AddAPIKey, EditAPIKey, DeleteAPIKey, GetGeminiKeys,
// GetInteractionsKeys, GetClaudeKeys, GetCodexKeys, GetXAIKeys,
// GetVertexKeys, GetOpenAICompat, getWrappedKeyList, extractList,
// GetAuthStatus, CancelAuthSession, PutBoolField, PutIntField,
// PutStringField) (v8.0.20, MIT), with how Go's net/url escapes a query
// (QueryEscape, Values.Encode; go1.26.4, BSD-3-Clause, see
// licenses/Go-LICENSE).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! The management API client the tabs share, and `open-ferry`'s agent
//! commands use (see [`Client::send`]).
//!
//! It calls `/v0/management` on the server's base URL with the management
//! key as a bearer token, and decodes responses as upstream decodes them
//! into Go maps. The key can change after the client is made, when the
//! auth gate takes one, and every tab sees the change.
//!
//! Upstream's `GetConfigYAML`, `PutConfigYAML`, `RefreshAllAuthFiles`,
//! `GetDebug` and `DeleteField` aren't ported: nothing in the TUI calls
//! them.
//!
//! Deviations from upstream:
//! - A response body over 32 MiB is an error rather than read whole.
//! - Control characters other than newlines are taken out of every string
//!   the server sends, and out of error text, before a view can show them,
//!   so a server can't write escape sequences to the terminal; tabs become
//!   spaces. Upstream shows them as they come.
//! - Transport errors read `Get "<url>": <cause>` as Go's do, but the cause
//!   is the one this HTTP stack gives, and JSON errors are worded as
//!   serde_json words them, except type mismatches, which read as Go's.
//! - A loopback base URL is never reached through a proxy from the
//!   environment, as Go's `ProxyFromEnvironment` never proxies one; other
//!   base URLs use the environment's proxy as reqwest reads it.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Method;
use serde_json::{Map, Value, json};

/// The largest response body read.
const MAX_BODY: usize = 32 << 20;

/// How long a request may take, as upstream's `http.Client` allows.
const TIMEOUT: Duration = Duration::from_secs(10);

/// A JSON object, as Go decodes one into `map[string]any`.
pub(crate) type Object = Map<String, Value>;

/// The management API client (upstream's `Client`). Outside the TUI,
/// `open-ferry`'s agent commands use it too, through [`Client::new`] and
/// [`Client::send`].
#[derive(Debug)]
pub struct Client {
    base_url: String,
    secret: Mutex<Secret>,
    http: reqwest::Client,
}

/// An answer [`Client::send`] got.
#[derive(Debug)]
pub struct Reply {
    /// The status code.
    pub status: u16,
    /// The `X-CPA-VERSION` header, which the server sets on the answers of
    /// an authenticated management request, cleaned of control characters.
    pub version: Option<String>,
    /// The body, at most 32 MiB.
    pub body: Vec<u8>,
}

/// The management key, kept out of `Debug` output.
#[derive(Default)]
struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

impl Client {
    /// `NewClient`: a client for the server on loopback port `port`.
    pub(crate) fn local(port: i64, secret: &str) -> Arc<Self> {
        Self::new(&format!("http://127.0.0.1:{port}"), secret)
    }

    /// `NewClientWithBaseURL`: a client for `base_url`, with `http://`
    /// added when it has no scheme and trailing slashes taken off; an empty
    /// one is `http://127.0.0.1:8317`.
    pub fn new(base_url: &str, secret: &str) -> Arc<Self> {
        let mut base = base_url.trim().to_owned();
        if base.is_empty() {
            base = "http://127.0.0.1:8317".to_owned();
        } else {
            let lower = base.to_lowercase();
            if !lower.starts_with("http://") && !lower.starts_with("https://") {
                base = format!("http://{base}");
            }
            base = base.trim_end_matches('/').to_owned();
        }
        let mut builder = reqwest::Client::builder().timeout(TIMEOUT);
        if is_loopback(&base) {
            builder = builder.no_proxy();
        }
        let http = builder.build().unwrap_or_default();
        Arc::new(Self {
            base_url: base,
            secret: Mutex::new(Secret(secret.trim().to_owned())),
            http,
        })
    }

    /// `BaseURL`.
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `SetSecretKey`: the key every later request sends.
    pub(crate) fn set_secret_key(&self, secret: &str) {
        let mut guard = self.secret.lock().unwrap_or_else(PoisonError::into_inner);
        guard.0 = secret.trim().to_owned();
    }

    fn secret(&self) -> String {
        self.secret
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .clone()
    }

    /// `doRequest`: the response's body and status.
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> Result<(Vec<u8>, u16), String> {
        let body = body.map(|body| ("application/json", body.into_bytes()));
        let reply = self.send(method, path, body).await?;
        Ok((reply.body, reply.status))
    }

    /// Sends `method path`, with `body` and its content type if given, and
    /// the key, and returns the answer whatever its status. Not upstream's:
    /// `open-ferry`'s agent commands call the management and dashboard APIs
    /// with it.
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<(&str, Vec<u8>)>,
    ) -> Result<Reply, String> {
        let url = format!("{}{path}", self.base_url);
        let op = go_op(&method);
        let mut req = self.http.request(method, &url);
        let secret = self.secret();
        if !secret.is_empty() {
            req = req.bearer_auth(secret);
        }
        if let Some((content_type, body)) = body {
            req = req
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| transport_error(op, &url, &e))?;
        let status = resp.status().as_u16();
        let version = resp
            .headers()
            .get("X-CPA-VERSION")
            .and_then(|value| value.to_str().ok())
            .map(clean);
        let mut data = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| transport_error(op, &url, &e))?;
            if data.len() + chunk.len() > MAX_BODY {
                return Err(format!("{op} {}: response body too large", go_quote(&url)));
            }
            data.extend_from_slice(&chunk);
        }
        Ok(Reply {
            status,
            version,
            body: data,
        })
    }

    /// `get`, `put` and `patch`: the body, or an error naming the status
    /// and the body for a status from 400.
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> Result<Vec<u8>, String> {
        let (data, code) = self.request(method, path, body).await?;
        if code >= 400 {
            return Err(format!(
                "HTTP {code}: {}",
                clean(String::from_utf8_lossy(&data).trim())
            ));
        }
        Ok(data)
    }

    /// `getJSON`: the map, `None` for a `null` body, as Go leaves a nil
    /// map.
    async fn get_nullable(&self, path: &str) -> Result<Option<Object>, String> {
        let data = self.call(Method::GET, path, None).await?;
        decode_object(&data)
    }

    /// `getJSON` where a nil map reads as an empty one.
    async fn get_json(&self, path: &str) -> Result<Object, String> {
        Ok(self.get_nullable(path).await?.unwrap_or_default())
    }

    /// `postJSON`: an error for a status from 400 names only the status.
    pub(crate) async fn post_json(&self, path: &str, body: &Value) -> Result<(), String> {
        let (_, code) = self
            .request(Method::POST, path, Some(body.to_string()))
            .await?;
        if code >= 400 {
            return Err(format!("HTTP {code}"));
        }
        Ok(())
    }

    /// `GetConfig`: `None` when the server sends `null`.
    pub(crate) async fn get_config(&self) -> Result<Option<Object>, String> {
        self.get_nullable("/v0/management/config").await
    }

    /// `GetAuthFiles`: the `files` list.
    pub(crate) async fn get_auth_files(&self) -> Result<Vec<Object>, String> {
        let wrapper = self.get_json("/v0/management/auth-files").await?;
        extract_list(&wrapper, "files")
    }

    /// `DeleteAuthFile`.
    pub(crate) async fn delete_auth_file(&self, name: &str) -> Result<(), String> {
        let path = format!("/v0/management/auth-files?{}", encode(&[("name", name)]));
        let (_, code) = self.request(Method::DELETE, &path, None).await?;
        if code >= 400 {
            return Err(format!("delete failed (HTTP {code})"));
        }
        Ok(())
    }

    /// `ToggleAuthFile`.
    pub(crate) async fn toggle_auth_file(&self, name: &str, disabled: bool) -> Result<(), String> {
        // Go marshals a map with its keys sorted.
        let body = json!({"disabled": disabled, "name": name});
        self.call(
            Method::PATCH,
            "/v0/management/auth-files/status",
            Some(body.to_string()),
        )
        .await
        .map(drop)
    }

    /// `PatchAuthFileFields`: `fields` with `name` added.
    pub(crate) async fn patch_auth_file_fields(
        &self,
        name: &str,
        mut fields: Object,
    ) -> Result<(), String> {
        fields.insert("name".to_owned(), Value::String(name.to_owned()));
        self.call(
            Method::PATCH,
            "/v0/management/auth-files/fields",
            Some(sorted(fields).to_string()),
        )
        .await
        .map(drop)
    }

    /// `RefreshAuthFile`.
    pub(crate) async fn refresh_auth_file(&self, name: &str) -> Result<(), String> {
        self.post_json("/v0/management/auth-files/refresh", &json!({"name": name}))
            .await
    }

    /// `GetLogs`: lines after the timestamp `after` (none if 0), at most
    /// `limit` (all if 0), and the latest timestamp, never before `after`.
    pub(crate) async fn get_logs(
        &self,
        after: i64,
        limit: i64,
    ) -> Result<(Vec<String>, i64), String> {
        let limit_text = limit.to_string();
        let after_text = after.to_string();
        let mut query = Vec::new();
        if after > 0 {
            query.push(("after", after_text.as_str()));
        }
        if limit > 0 {
            query.push(("limit", limit_text.as_str()));
        }
        let mut path = "/v0/management/logs".to_owned();
        let encoded = encode(&query);
        if !encoded.is_empty() {
            path.push('?');
            path.push_str(&encoded);
        }
        let wrapper = self.get_json(&path).await?;
        let lines = match wrapper.get("lines") {
            None | Some(Value::Null) => Vec::new(),
            Some(raw) => strings(raw)?,
        };
        let mut latest = after;
        if let Some(Value::Number(n)) = wrapper.get("latest-timestamp")
            && let Some(f) = n.as_f64()
        {
            // Go converts the float64 to an int64.
            latest = f as i64;
        }
        Ok((lines, latest.max(after)))
    }

    /// `GetAPIKeys`.
    pub(crate) async fn get_api_keys(&self) -> Result<Vec<String>, String> {
        let wrapper = self.get_json("/v0/management/api-keys").await?;
        match wrapper.get("api-keys") {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(raw) => strings(raw),
        }
    }

    /// `AddAPIKey`: appends `key`.
    pub(crate) async fn add_api_key(&self, key: &str) -> Result<(), String> {
        let body = json!({"new": key, "old": null});
        self.call(
            Method::PATCH,
            "/v0/management/api-keys",
            Some(body.to_string()),
        )
        .await
        .map(drop)
    }

    /// `EditAPIKey`: replaces the key at `index`.
    pub(crate) async fn edit_api_key(&self, index: usize, value: &str) -> Result<(), String> {
        let body = json!({"index": index, "value": value});
        self.call(
            Method::PATCH,
            "/v0/management/api-keys",
            Some(body.to_string()),
        )
        .await
        .map(drop)
    }

    /// `DeleteAPIKey`: deletes the key at `index`.
    pub(crate) async fn delete_api_key(&self, index: usize) -> Result<(), String> {
        let path = format!("/v0/management/api-keys?index={index}");
        let (_, code) = self.request(Method::DELETE, &path, None).await?;
        if code >= 400 {
            return Err(format!("delete failed (HTTP {code})"));
        }
        Ok(())
    }

    /// `getWrappedKeyList`, behind `GetGeminiKeys`, `GetInteractionsKeys`,
    /// `GetClaudeKeys`, `GetCodexKeys`, `GetXAIKeys`, `GetVertexKeys` and
    /// `GetOpenAICompat`: the list under `key` at `/v0/management/<key>`.
    pub(crate) async fn get_key_list(&self, key: &str) -> Result<Vec<Object>, String> {
        let wrapper = self.get_json(&format!("/v0/management/{key}")).await?;
        extract_list(&wrapper, key)
    }

    /// `GetAuthStatus`: the OAuth session's status and error message.
    pub(crate) async fn get_auth_status(&self, state: &str) -> Result<(String, String), String> {
        let path = format!(
            "/v0/management/get-auth-status?{}",
            encode(&[("state", state)])
        );
        let wrapper = self.get_json(&path).await?;
        Ok((
            get_string(&wrapper, "status"),
            get_string(&wrapper, "error"),
        ))
    }

    /// `CancelAuthSession`.
    pub(crate) async fn cancel_auth_session(&self, state: &str) -> Result<(), String> {
        let state = state.trim();
        if state.is_empty() {
            return Ok(());
        }
        let path = format!(
            "/v0/management/oauth-session?{}",
            encode(&[("state", state)])
        );
        let (_, code) = self.request(Method::DELETE, &path, None).await?;
        if code >= 400 {
            return Err(format!("HTTP {code}"));
        }
        Ok(())
    }

    /// `PutBoolField`, `PutIntField` and `PutStringField`: sets the config
    /// field at `path` to `value`.
    pub(crate) async fn put_field(&self, path: &str, value: Value) -> Result<(), String> {
        let body = json!({ "value": value });
        self.call(
            Method::PUT,
            &format!("/v0/management/{path}"),
            Some(body.to_string()),
        )
        .await
        .map(drop)
    }

    /// An absolute URL on the server, for upstream's requests that the OAuth
    /// tab builds itself.
    pub(crate) async fn get_json_path(&self, path: &str) -> Result<Object, String> {
        self.get_json(path).await
    }
}

/// `getString` as the tabs use it: the string under `key`, else "".
pub(crate) fn get_string(m: &Object, key: &str) -> String {
    match m.get(key) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Go's `url.Error` operation name for a method.
fn go_op(method: &Method) -> &'static str {
    match *method {
        Method::GET => "Get",
        Method::POST => "Post",
        Method::PUT => "Put",
        Method::PATCH => "Patch",
        Method::DELETE => "Delete",
        _ => "Do",
    }
}

/// A Go-quoted string, as `url.Error` quotes its URL.
fn go_quote(s: &str) -> String {
    open_ferry_translate::go::quote(s)
}

/// The text of a transport error, as Go's `url.Error` reads:
/// `Get "<url>": <cause>`.
fn transport_error(op: &str, url: &str, err: &reqwest::Error) -> String {
    let mut cause: &dyn std::error::Error = err;
    while let Some(inner) = cause.source() {
        cause = inner;
    }
    let reason = if err.is_timeout() {
        "context deadline exceeded (Client.Timeout exceeded while awaiting headers)".to_owned()
    } else {
        cause.to_string()
    };
    clean(&format!("{op} {}: {reason}", go_quote(url)))
}

/// Whether a base URL's host is a loopback address or `localhost`.
fn is_loopback(base: &str) -> bool {
    let rest = base.split_once("://").map_or(base, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(v6) = host_port.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// Takes control characters other than newlines out of server text; tabs
/// become spaces.
pub(crate) fn clean(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\n' => Some('\n'),
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// [`clean`] applied to every string in a JSON value, keys included.
fn clean_value(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(clean(&s)),
        Value::Array(items) => Value::Array(items.into_iter().map(clean_value).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (clean(&k), clean_value(v)))
                .collect(),
        ),
        other => other,
    }
}

/// Decodes a body into a map, as `json.Unmarshal` into `map[string]any`
/// does: `null` leaves the map nil, here `None`.
fn decode_object(data: &[u8]) -> Result<Option<Object>, String> {
    let value: Value = serde_json::from_slice(data).map_err(|e| clean(&e.to_string()))?;
    match clean_value(value) {
        Value::Object(map) => Ok(Some(map)),
        Value::Null => Ok(None),
        other => Err(type_error(&other, "map[string]interface {}")),
    }
}

/// Go's `json.UnmarshalTypeError` text.
fn type_error(value: &Value, go_type: &str) -> String {
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    format!("json: cannot unmarshal {kind} into Go value of type {go_type}")
}

/// A list of strings, as Go decodes one into `[]string`; a `null` item is
/// "".
fn strings(raw: &Value) -> Result<Vec<String>, String> {
    let Value::Array(items) = raw else {
        return Err(type_error(raw, "[]string"));
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(s) => Ok(s.clone()),
            Value::Null => Ok(String::new()),
            other => Err(type_error(other, "string")),
        })
        .collect()
}

/// `extractList`: the list of objects under `key`, none if it is missing
/// or `null`; a `null` item is an empty object.
fn extract_list(wrapper: &Object, key: &str) -> Result<Vec<Object>, String> {
    match wrapper.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Object(m) => Ok(m.clone()),
                Value::Null => Ok(Object::new()),
                other => Err(type_error(other, "map[string]interface {}")),
            })
            .collect(),
        Some(other) => Err(type_error(other, "[]map[string]interface {}")),
    }
}

/// An object with its keys sorted, as Go marshals a map.
fn sorted(map: Object) -> Value {
    let mut entries: Vec<(String, Value)> = map.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Object(entries.into_iter().collect())
}

/// Go's `url.Values.Encode` for one value per key: keys sorted, each
/// `key=value` query-escaped.
pub(crate) fn encode(pairs: &[(&str, &str)]) -> String {
    let mut pairs = pairs.to_vec();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = String::new();
    for (k, v) in pairs {
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(&query_escape(k));
        out.push('=');
        out.push_str(&query_escape(v));
    }
    out
}

/// Go's `url.QueryEscape`.
pub(crate) fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            b' ' => out.push('+'),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Server;

    // Ports TestNewClientWithBaseURL.
    #[test]
    fn new_client_with_base_url() {
        for (input, expected) in [
            ("http://192.168.1.100:8317", "http://192.168.1.100:8317"),
            ("https://proxy.example.com/", "https://proxy.example.com"),
            ("HTTPS://proxy.example.com/", "HTTPS://proxy.example.com"),
            ("proxy.example.com:9000", "http://proxy.example.com:9000"),
            (
                "https://proxy.example.com/prefix/",
                "https://proxy.example.com/prefix",
            ),
            ("", "http://127.0.0.1:8317"),
        ] {
            assert_eq!(Client::new(input, "secret").base_url(), expected, "{input}");
        }
    }

    // Ports TestNewClient_BackwardsCompatibility.
    #[test]
    fn new_client_backwards_compatibility() {
        assert_eq!(
            Client::local(8317, "test-secret").base_url(),
            "http://127.0.0.1:8317"
        );
    }

    // Ports TestClient_RemoteServerInteraction.
    #[tokio::test]
    async fn client_remote_server_interaction() {
        let server = Server::start(&[("GET /v0/management/config", r#"{"status":"ok"}"#)]).await;
        let client = Client::new(&server.url(), "remote-secret-key");
        let cfg = client.get_config().await.unwrap().unwrap();
        assert_eq!(cfg.get("status"), Some(&Value::from("ok")));
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/v0/management/config");
        assert_eq!(requests[0].auth, "Bearer remote-secret-key");
    }

    // Not upstream's: requests carry upstream's methods, paths, query
    // strings and bodies, and the key set after the client was made.
    #[tokio::test]
    async fn sends_requests_as_upstream_does() {
        let server = Server::start(&[]).await;
        let client = Client::new(&server.url(), "");
        let _ = client.get_logs(0, 200).await;
        client.set_secret_key("  k  ");
        let _ = client.get_logs(1_767_225_603, 50).await;
        let _ = client.delete_auth_file("a b&c~*.json").await;
        let _ = client.toggle_auth_file("x.json", true).await;
        let mut fields = Object::new();
        fields.insert("prefix".into(), Value::from("p"));
        fields.insert("priority".into(), Value::from(2));
        let _ = client.patch_auth_file_fields("x.json", fields).await;
        let _ = client.refresh_auth_file("x.json").await;
        let _ = client.add_api_key("new").await;
        let _ = client.edit_api_key(1, "v").await;
        let _ = client.delete_api_key(2).await;
        let _ = client.get_auth_status("st 1").await;
        let _ = client.cancel_auth_session("  ").await;
        let _ = client.cancel_auth_session(" st-2 ").await;
        let _ = client.put_field("debug", Value::Bool(true)).await;
        let got: Vec<String> = server.requests().iter().map(ToString::to_string).collect();
        assert_eq!(
            got,
            [
                "GET /v0/management/logs?limit=200 auth= body=",
                "GET /v0/management/logs?after=1767225603&limit=50 auth=Bearer k body=",
                "DELETE /v0/management/auth-files?name=a+b%26c~%2A.json auth=Bearer k body=",
                r#"PATCH /v0/management/auth-files/status auth=Bearer k body={"disabled":true,"name":"x.json"}"#,
                r#"PATCH /v0/management/auth-files/fields auth=Bearer k body={"name":"x.json","prefix":"p","priority":2}"#,
                r#"POST /v0/management/auth-files/refresh auth=Bearer k body={"name":"x.json"}"#,
                r#"PATCH /v0/management/api-keys auth=Bearer k body={"new":"new","old":null}"#,
                r#"PATCH /v0/management/api-keys auth=Bearer k body={"index":1,"value":"v"}"#,
                "DELETE /v0/management/api-keys?index=2 auth=Bearer k body=",
                "GET /v0/management/get-auth-status?state=st+1 auth=Bearer k body=",
                "DELETE /v0/management/oauth-session?state=st-2 auth=Bearer k body=",
                r#"PUT /v0/management/debug auth=Bearer k body={"value":true}"#,
            ]
        );
    }

    // Not upstream's: responses decode as Go decodes them, errors read as
    // upstream's, and server text is cleaned.
    #[tokio::test]
    async fn decodes_responses_as_upstream_does() {
        let server = Server::start(&[
            ("GET /v0/management/config", "null"),
            (
                "GET /v0/management/auth-files",
                r#"{"files":[{"name":"a\u001b[31m\tb"},null]}"#,
            ),
            ("GET /v0/management/api-keys", r#"{"api-keys":["k",null]}"#),
            (
                "GET /v0/management/gemini-api-key",
                r#"{"gemini-api-key":"x"}"#,
            ),
            ("GET /v0/management/claude-api-key", r#"{}"#),
            (
                "GET /v0/management/logs",
                r#"{"lines":null,"latest-timestamp":5}"#,
            ),
            ("GET /v0/management/codex-api-key", "[1]"),
        ])
        .await;
        let client = Client::new(&server.url(), "k");
        assert_eq!(client.get_config().await.unwrap(), None);
        let files = client.get_auth_files().await.unwrap();
        assert_eq!(files[0].get("name"), Some(&Value::from("a[31m b")));
        assert!(files[1].is_empty());
        assert_eq!(client.get_api_keys().await.unwrap(), ["k", ""]);
        assert_eq!(
            client.get_key_list("gemini-api-key").await.unwrap_err(),
            "json: cannot unmarshal string into Go value of type []map[string]interface {}"
        );
        assert!(
            client
                .get_key_list("claude-api-key")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(client.get_logs(9, 0).await.unwrap(), (vec![], 9));
        assert_eq!(client.get_logs(0, 0).await.unwrap(), (vec![], 5));
        assert_eq!(
            client.get_key_list("codex-api-key").await.unwrap_err(),
            "json: cannot unmarshal array into Go value of type map[string]interface {}"
        );
        assert_eq!(
            client.get_key_list("vertex-api-key").await.unwrap_err(),
            r#"HTTP 404: {"error":"not found"}"#
        );
        assert_eq!(
            client.delete_api_key(0).await.unwrap_err(),
            "delete failed (HTTP 404)"
        );
        assert_eq!(client.refresh_auth_file("x").await.unwrap_err(), "HTTP 404");
    }

    // Not upstream's: a body over the cap is refused, and a closed port
    // gives Go's shape of transport error.
    #[tokio::test]
    async fn refuses_huge_bodies_and_reports_transport_errors() {
        let big = format!(r#"{{"lines":["{}"]}}"#, "x".repeat(MAX_BODY));
        let server = Server::start(&[("GET /v0/management/logs", big.as_str())]).await;
        let client = Client::new(&server.url(), "k");
        let err = client.get_logs(0, 0).await.unwrap_err();
        assert!(err.ends_with("response body too large"), "{err}");

        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = socket.local_addr().unwrap().port();
        let client = Client::local(i64::from(port), "k");
        let err = client.get_config().await.unwrap_err();
        let prefix = format!("Get \"http://127.0.0.1:{port}/v0/management/config\": ");
        assert!(err.starts_with(&prefix), "{err}");
    }

    // Not upstream's: queries escape as Go's url.QueryEscape escapes them.
    #[test]
    fn escapes_queries_as_go_does() {
        assert_eq!(query_escape("a b+c/~*é"), "a+b%2Bc%2F~%2A%C3%A9");
        assert_eq!(encode(&[("limit", "1"), ("after", "2")]), "after=2&limit=1");
        assert!(is_loopback("http://127.0.0.1:8317"));
        assert!(is_loopback("http://LOCALHOST"));
        assert!(is_loopback("http://[::1]:9/x"));
        assert!(is_loopback("http://u:p@127.0.0.2"));
        assert!(!is_loopback("https://proxy.example.com"));
    }
}
