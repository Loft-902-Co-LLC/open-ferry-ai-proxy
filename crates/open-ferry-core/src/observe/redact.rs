//! Keeps secrets out of what leaves the proxy: the errors made from an
//! upstream's answers, the request log's files and the usage records'
//! failures.
//!
//! A [`Secrets`] is a set of secrets, gathered from what an upstream attempt
//! actually sent: its credential headers and each cookie value
//! ([`Secrets::add_headers`]), the credentials in its URL
//! ([`Secrets::add_url`]), its proxy's password ([`Secrets::add_proxy`]) and
//! its credential's own keys and tokens ([`Secrets::add_auth`]). Every copy
//! of each, as it is or escaped as a JSON string, becomes [`REDACTED`], in
//! one pass, the longest winning where two overlap.
//!
//! An upstream's error body, or the payload of an error in its stream,
//! reaches the client as the error's message. A provider that quotes the key
//! it was sent ("Invalid API key: sk-...") would hand the proxy's key to
//! whoever called it, so what a client is given is scrubbed with
//! [`Policy::Client`], which leaves a secret shorter than eight bytes alone:
//! it could be an ordinary word of the message, and hiding it would garble
//! the message. What is written to disk is scrubbed with [`Policy::Disk`],
//! which hides every secret, however short.
//!
//! Deviations from upstream: the whole module. Upstream passes error bodies
//! on as they came, and writes its logs with the secrets in them.

use std::borrow::Cow;
use std::fmt;
use std::sync::OnceLock;

use aho_corasick::{AhoCorasick, Input, MatchKind};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::HeaderMap;
use serde_json::{Map, Value};

use super::mask;
use crate::auth::Auth;

/// What a secret becomes.
pub const REDACTED: &str = "[redacted]";

/// The shortest secret [`Policy::Client`] redacts. A shorter one could be
/// an ordinary word of the message, and hiding it would garble the message.
const MIN_SECRET: usize = 8;

/// The metadata keys a credential keeps its keys and tokens under, at the
/// top level or in a nested `token` object.
const METADATA_SECRETS: [&str; 8] = [
    "api_key",
    "apiKey",
    "access_token",
    "accessToken",
    "refresh_token",
    "refreshToken",
    "id_token",
    "idToken",
];

/// The environment variables an empty proxy setting reads its proxy from,
/// as reqwest and Go's default transport do.
const PROXY_VARIABLES: [&str; 6] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// Which secrets a scrub hides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// For what a client is given: secrets of eight bytes or more.
    Client,
    /// For what is written to disk: every secret, however short.
    Disk,
}

/// A set of secrets to scrub. `Debug` shows only how many it holds.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secrets(Vec<String>, Searchers);

/// The searchers [`Secrets::bytes`] built, one for each policy, kept so a
/// stream scrubbed line by line builds each once. Adding a secret drops
/// them. They don't count in equality.
#[derive(Clone, Default)]
struct Searchers([OnceLock<Searcher>; 2]);

impl Searchers {
    const fn new() -> Self {
        Self([OnceLock::new(), OnceLock::new()])
    }

    /// `policy`'s searcher, built by `build` the first time.
    fn get(&self, policy: Policy, build: impl FnOnce() -> Searcher) -> &Searcher {
        let [client, disk] = &self.0;
        match policy {
            Policy::Client => client,
            Policy::Disk => disk,
        }
        .get_or_init(build)
    }
}

impl PartialEq for Searchers {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Searchers {}

/// The forms of the secrets one policy hides, and the automaton that finds
/// them (`None` when there are none, or it couldn't be built).
#[derive(Clone)]
struct Searcher {
    patterns: Vec<String>,
    automaton: Option<AhoCorasick>,
}

impl Searcher {
    fn new(patterns: Vec<String>) -> Self {
        let automaton = if patterns.is_empty() {
            None
        } else {
            AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostLongest)
                .build(&patterns)
                .ok()
        };
        Self {
            patterns,
            automaton,
        }
    }
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secrets({})", self.0.len())
    }
}

impl<S: AsRef<str>> FromIterator<S> for Secrets {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        let mut secrets = Self::default();
        for secret in iter {
            secrets.add(secret.as_ref());
        }
        secrets
    }
}

impl Secrets {
    /// An empty set.
    pub const fn new() -> Self {
        Self(Vec::new(), Searchers::new())
    }

    /// Whether it holds no secret.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The secrets, in the order they were added.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// Adds `secret`, without the white space around it; nothing when that
    /// leaves it empty or it is held already.
    pub fn add(&mut self, secret: &str) {
        let secret = secret.trim();
        if !secret.is_empty() && !self.0.iter().any(|kept| kept == secret) {
            self.0.push(secret.to_owned());
            self.1 = Searchers::default();
        }
    }

    /// Adds every secret of `other`.
    pub fn extend(&mut self, other: &Secrets) {
        for secret in &other.0 {
            self.add(secret);
        }
    }

    /// Adds the secrets `headers` carry (see [`Self::add_header`]).
    pub fn add_headers(&mut self, headers: &HeaderMap) {
        for (name, value) in headers {
            self.add_header(name.as_str(), value.as_bytes());
        }
    }

    /// Adds the secrets a header named `name` carries in `value`: each
    /// cookie's value of a `Cookie`, the cookie's value of a `Set-Cookie`,
    /// the credential of an `Authorization` or `Proxy-Authorization` (and a
    /// `Basic` one's password), and the whole value of any other credential
    /// header: one whose name holds `api-key`, `apikey`, `api_key`,
    /// `secret` or `password`, or ends in `key` or `token`, such as
    /// `X-Api-Key`, `X-Goog-Api-Key` and `X-Management-Key`. A value that
    /// isn't UTF-8 is skipped.
    pub fn add_header(&mut self, name: &str, value: &[u8]) {
        let Ok(value) = std::str::from_utf8(value) else {
            return;
        };
        let name = name.trim().to_ascii_lowercase();
        match name.as_str() {
            "cookie" => {
                for pair in value.split(';') {
                    self.add_cookie(pair);
                }
            }
            "set-cookie" => self.add_cookie(value.split(';').next().unwrap_or_default()),
            _ if name.contains("authorization") => self.add_authorization(value),
            _ if is_secret_header(&name) => self.add(value),
            _ => {}
        }
    }

    /// Adds the secrets in `url`: its user info's password (its user, when
    /// it has no password) and the `Basic` credential an HTTP client makes
    /// of the user info, for an `Authorization` or, for a proxy's URL, a
    /// `Proxy-Authorization`; and the values of its key-like query
    /// parameters, as [`mask::mask_sensitive_query`] tells them, or named
    /// for a password; each as it is and percent-decoded. A path with a
    /// query, without a scheme or host, is read too.
    pub fn add_url(&mut self, url: &str) {
        if let Some(info) = user_info(url) {
            self.add_user_info(info, true);
            let (user, password) = info.split_once(':').unwrap_or((info, ""));
            let decode = |part: &str| {
                mask::percent_unescape(part).map_or_else(
                    || part.to_owned(),
                    |bytes| String::from_utf8_lossy(&bytes).into_owned(),
                )
            };
            self.add(&STANDARD.encode(format!("{}:{}", decode(user), decode(password))));
        }
        let url = url.trim();
        let rest = url.split_once('#').map_or(url, |(rest, _)| rest);
        let query = rest.split_once('?').map_or("", |(_, query)| query);
        for part in query.split('&') {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            let key = mask::query_unescape(key)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_else(|| key.to_owned());
            if !mask::should_mask_query_param(&key)
                && !key.to_ascii_lowercase().contains("password")
            {
                continue;
            }
            self.add(value);
            if let Some(decoded) =
                mask::query_unescape(value).and_then(|bytes| String::from_utf8(bytes).ok())
            {
                self.add(&decoded);
            }
        }
    }

    /// Adds the password of the proxy a request with the proxy setting
    /// `proxy` goes through, and the `Proxy-Authorization` credential made
    /// of it (see [`Self::add_url`]): a proxy URL's; none for `direct` or
    /// `none`; and for an empty
    /// setting, the environment's proxies' (`HTTP_PROXY`, `HTTPS_PROXY` and
    /// `ALL_PROXY`, in either case).
    pub fn add_proxy(&mut self, proxy: &str) {
        let proxy = proxy.trim();
        if proxy.eq_ignore_ascii_case("direct") || proxy.eq_ignore_ascii_case("none") {
            return;
        }
        if !proxy.is_empty() {
            self.add_url(proxy);
            return;
        }
        for name in PROXY_VARIABLES {
            if let Ok(value) = std::env::var(name) {
                let value = value.trim();
                if value.contains("://") {
                    self.add_url(value);
                } else if !value.is_empty() {
                    self.add_url(&format!("http://{value}"));
                }
            }
        }
    }

    /// Adds `auth`'s own secrets: its `api_key` attribute, the credential
    /// headers among its custom headers (its `header:<name>` attributes,
    /// read as [`Self::add_header`] reads them), its proxy URL's password
    /// (see [`Self::add_proxy`]), and the keys and tokens its metadata
    /// keeps, at the top level or in a
    /// nested `token` object (`api_key`, `access_token`, `refresh_token`
    /// and `id_token`, in snake or camel case).
    pub fn add_auth(&mut self, auth: &Auth) {
        if let Some(key) = auth.attributes.get("api_key") {
            self.add(key);
        }
        for (name, value) in &auth.attributes {
            if let Some(header) = name.strip_prefix("header:") {
                self.add_header(header, value.as_bytes());
            }
        }
        self.add_url(&auth.proxy_url);
        self.add_tokens(&auth.metadata);
        for nested in ["token", "Token"] {
            if let Some(Value::Object(object)) = auth.metadata.get(nested) {
                self.add_tokens(object);
            }
        }
    }

    /// `body` with every copy of each secret `policy` hides replaced by
    /// [`REDACTED`].
    pub fn bytes<'a>(&self, body: &'a [u8], policy: Policy) -> Cow<'a, [u8]> {
        if body.is_empty() {
            return Cow::Borrowed(body);
        }
        let searcher = self.1.get(policy, || Searcher::new(self.patterns(policy)));
        if searcher.patterns.is_empty() {
            return Cow::Borrowed(body);
        }
        let Some(Ok(matches)) = searcher
            .automaton
            .as_ref()
            .map(|automaton| automaton.try_find_iter(Input::new(body)))
        else {
            return replace_each(body, searcher.patterns.clone());
        };
        let mut out: Option<Vec<u8>> = None;
        let mut last = 0;
        for found in matches {
            let buffer = out.get_or_insert_with(|| Vec::with_capacity(body.len()));
            buffer.extend_from_slice(body.get(last..found.start()).unwrap_or_default());
            buffer.extend_from_slice(REDACTED.as_bytes());
            last = found.end();
        }
        match out {
            None => Cow::Borrowed(body),
            Some(mut out) => {
                out.extend_from_slice(body.get(last..).unwrap_or_default());
                Cow::Owned(out)
            }
        }
    }

    /// `text` with every copy of each secret `policy` hides replaced by
    /// [`REDACTED`].
    pub fn text(&self, text: String, policy: Policy) -> String {
        let replaced = match self.bytes(text.as_bytes(), policy) {
            Cow::Borrowed(_) => None,
            Cow::Owned(bytes) => Some(bytes),
        };
        match replaced {
            None => text,
            Some(bytes) => into_string(bytes),
        }
    }

    /// `text` with every copy of each secret `policy` hides replaced by
    /// [`REDACTED`], borrowed when it has none.
    pub fn str<'a>(&self, text: &'a str, policy: Policy) -> Cow<'a, str> {
        match self.bytes(text.as_bytes(), policy) {
            Cow::Borrowed(_) => Cow::Borrowed(text),
            Cow::Owned(bytes) => Cow::Owned(into_string(bytes)),
        }
    }

    /// The forms of the secrets `policy` hides: each as it is, and escaped
    /// as a JSON string where that differs.
    fn patterns(&self, policy: Policy) -> Vec<String> {
        let mut patterns: Vec<String> = Vec::new();
        for secret in &self.0 {
            if policy == Policy::Client && secret.len() < MIN_SECRET {
                continue;
            }
            for form in forms(secret) {
                if !patterns.contains(&form) {
                    patterns.push(form);
                }
            }
        }
        patterns
    }

    fn add_cookie(&mut self, pair: &str) {
        let value = pair.split_once('=').map_or(pair, |(_, value)| value).trim();
        self.add(value);
        if let Some(unquoted) = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
        {
            self.add(unquoted);
        }
    }

    fn add_authorization(&mut self, value: &str) {
        let value = value.trim();
        let Some((scheme, credential)) = value.split_once(|c: char| c.is_ascii_whitespace()) else {
            self.add(value);
            return;
        };
        let credential = credential.trim();
        self.add(credential);
        if scheme.eq_ignore_ascii_case("basic")
            && let Some(decoded) = STANDARD
                .decode(credential)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
        {
            self.add_user_info(&decoded, false);
        }
    }

    /// Adds the secret of `user:password` user info: the password, or the
    /// user when there is none; also percent-decoded when `encoded`.
    fn add_user_info(&mut self, info: &str, encoded: bool) {
        let secret = match info.split_once(':') {
            Some((_, password)) if !password.is_empty() => password,
            Some((user, _)) => user,
            None => info,
        };
        self.add(secret);
        if encoded
            && let Some(decoded) =
                mask::percent_unescape(secret).and_then(|bytes| String::from_utf8(bytes).ok())
        {
            self.add(&decoded);
        }
    }

    fn add_tokens(&mut self, object: &Map<String, Value>) {
        for key in METADATA_SECRETS {
            if let Some(Value::String(secret)) = object.get(key) {
                self.add(secret);
            }
        }
    }
}

/// The user info of `url`, as it is written: what comes before an `@` in its
/// authority. `None` without a scheme or user info.
fn user_info(url: &str) -> Option<&str> {
    let url = url.trim();
    let rest = url.split_once('#').map_or(url, |(rest, _)| rest);
    let base = rest.split_once('?').map_or(rest, |(base, _)| base);
    let (_, after) = base.split_once("://")?;
    let end = after.find(['/', '\\']).unwrap_or(after.len());
    let (authority, _) = after.split_at_checked(end)?;
    authority.rsplit_once('@').map(|(info, _)| info)
}

/// Whether a header named `name`, in lower case, other than a cookie or
/// authorization header, carries a credential as its whole value.
fn is_secret_header(name: &str) -> bool {
    ["api-key", "apikey", "api_key", "secret", "password"]
        .iter()
        .any(|part| name.contains(part))
        || name.ends_with("key")
        || name.ends_with("token")
}

/// `bytes`, which came from UTF-8 text with whole UTF-8 strings replaced,
/// as text.
fn into_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

/// `body` with every copy of each of `patterns` replaced, longest first:
/// the fallback for a set the one-pass searcher can't be built for.
fn replace_each(body: &[u8], mut patterns: Vec<String>) -> Cow<'_, [u8]> {
    patterns.sort_by_key(|pattern| std::cmp::Reverse(pattern.len()));
    let mut out = Cow::Borrowed(body);
    for pattern in &patterns {
        if let Some(replaced) = replace(&out, pattern.as_bytes()) {
            out = Cow::Owned(replaced);
        }
    }
    out
}

/// `body` with every copy of `secret` that [`Policy::Client`] hides
/// replaced by [`REDACTED`].
pub fn bytes<'a>(body: &'a [u8], secret: &str) -> Cow<'a, [u8]> {
    Secrets::from_iter([secret]).bytes(body, Policy::Client)
}

/// `text` with every copy of `secret` that [`Policy::Client`] hides
/// replaced by [`REDACTED`].
pub fn text(text: String, secret: &str) -> String {
    Secrets::from_iter([secret]).text(text, Policy::Client)
}

/// The forms of `secret` to look for: itself, and its JSON-escaped form if
/// that differs.
fn forms(secret: &str) -> Vec<String> {
    let mut forms = vec![secret.to_owned()];
    let quoted = serde_json::to_string(secret).unwrap_or_default();
    if let Some(escaped) = quoted
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .filter(|escaped| *escaped != secret)
    {
        forms.push(escaped.to_owned());
    }
    forms
}

/// `body` with every copy of `needle` replaced, or `None` when it has none
/// or `needle` is empty.
fn replace(body: &[u8], needle: &[u8]) -> Option<Vec<u8>> {
    if needle.is_empty() {
        return None;
    }
    let mut rest = body;
    let mut out: Option<Vec<u8>> = None;
    while let Some(at) = rest
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let (before, after) = rest.split_at_checked(at)?;
        let buffer = out.get_or_insert_with(|| Vec::with_capacity(body.len()));
        buffer.extend_from_slice(before);
        buffer.extend_from_slice(REDACTED.as_bytes());
        rest = after.get(needle.len()..)?;
    }
    let mut out = out?;
    out.extend_from_slice(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;
    use serde_json::json;

    use super::*;

    #[test]
    fn replaces_every_copy_of_the_secret() {
        let body = br#"{"error":"Invalid API key: Bearer sk-secret-1234 (sk-secret-1234)"}"#;
        assert_eq!(
            bytes(body, "sk-secret-1234").as_ref(),
            br#"{"error":"Invalid API key: Bearer [redacted] ([redacted])"}"#
        );
        assert_eq!(
            text("key sk-secret-1234.".to_owned(), "sk-secret-1234"),
            "key [redacted]."
        );
    }

    #[test]
    fn a_secret_added_after_a_scrub_is_hidden_by_the_next() {
        let mut secrets = Secrets::from_iter(["first-secret-1234"]);
        let body = "first-secret-1234 then second-secret-5678";
        assert_eq!(
            secrets.text(body.to_owned(), Policy::Client),
            "[redacted] then second-secret-5678"
        );
        assert_eq!(secrets.text("short".to_owned(), Policy::Disk), "short");
        secrets.add("second-secret-5678");
        secrets.add("short");
        assert_eq!(
            secrets.text(body.to_owned(), Policy::Client),
            "[redacted] then [redacted]"
        );
        assert_eq!(
            secrets.clone().text("short".to_owned(), Policy::Disk),
            "[redacted]"
        );
        assert_eq!(secrets.clone(), secrets);
    }

    #[test]
    fn replaces_the_json_escaped_secret() {
        let secret = "abc/def\"ghi";
        let body = br#"{"error":"bad key abc/def\"ghi"}"#;
        assert_eq!(
            bytes(body, secret).as_ref(),
            br#"{"error":"bad key [redacted]"}"#
        );
    }

    #[test]
    fn leaves_bodies_without_the_secret_alone() {
        let body = b"no key here";
        assert!(matches!(bytes(body, "sk-secret-1234"), Cow::Borrowed(_)));
        assert!(matches!(bytes(b"", "sk-secret-1234"), Cow::Borrowed(_)));
        // Too short to be told from an ordinary word.
        assert_eq!(bytes(b"key short", "short").as_ref(), b"key short");
        assert_eq!(text("key ".to_owned(), ""), "key ");
    }

    #[test]
    fn keeps_text_around_multibyte_characters() {
        assert_eq!(
            text("é sk-secret-1234 ü".to_owned(), "sk-secret-1234"),
            "é [redacted] ü"
        );
        assert_eq!(
            bytes(b"\xff sk-secret-1234 \xfe", "sk-secret-1234").as_ref(),
            b"\xff [redacted] \xfe"
        );
    }

    // Not upstream's: the disk policy hides a secret of any length, where
    // the client's leaves one shorter than eight bytes.
    #[test]
    fn the_disk_policy_hides_short_secrets() {
        let secrets = Secrets::from_iter(["abc123", "x"]);
        assert_eq!(
            secrets.text("key abc123 and x".to_owned(), Policy::Disk),
            "key [redacted] and [redacted]"
        );
        assert_eq!(
            secrets.text("key abc123 and x".to_owned(), Policy::Client),
            "key abc123 and x"
        );
    }

    // Not upstream's: overlapping secrets are hidden in one pass, the
    // longest winning, and nothing inside a replacement is replaced again.
    #[test]
    fn hides_overlapping_secrets_in_one_pass() {
        let secrets = Secrets::from_iter(["secret-1234", "secret-1234-long", "dact"]);
        assert_eq!(
            secrets.str("a secret-1234-long b secret-1234 c", Policy::Disk),
            "a [redacted] b [redacted] c"
        );
        assert!(matches!(
            secrets.str("nothing here", Policy::Disk),
            Cow::Borrowed(_)
        ));
    }

    // Not upstream's: the secrets of credential headers, both directions'
    // cookies and URLs are gathered, and nothing else.
    #[test]
    fn gathers_the_secrets_of_headers_and_urls() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("authorization", "Bearer override-secret-5678"),
            ("proxy-authorization", "Basic dXNlcjpwcm94eS1wYXNz"),
            ("x-api-key", "second-key-0123"),
            ("x-goog-api-key", "goog-key-0123"),
            ("api-key", "azure-key-0123"),
            ("x-management-key", "management-secret"),
            ("cookie", "sid=cookie-secret-9012; theme=\"dark-quoted\""),
            ("set-cookie", "__cf=cf-secret-1; Path=/; HttpOnly"),
            ("x-ratelimit-remaining-tokens", "39000"),
            ("content-type", "application/json"),
        ] {
            headers.append(
                http::HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        let mut secrets = Secrets::new();
        secrets.add_headers(&headers);
        secrets
            .add_url("https://user:p%40ss@example.test/v1?key=AIza-1&alt=sse&access_token=a%2Bb");
        secrets.add_url("/v1/models?api_key=client-key");
        assert_eq!(
            secrets.iter().collect::<Vec<_>>(),
            [
                "override-secret-5678",
                "dXNlcjpwcm94eS1wYXNz",
                "proxy-pass",
                "second-key-0123",
                "goog-key-0123",
                "azure-key-0123",
                "management-secret",
                "cookie-secret-9012",
                "\"dark-quoted\"",
                "dark-quoted",
                "cf-secret-1",
                "p%40ss",
                "p@ss",
                "dXNlcjpwQHNz",
                "AIza-1",
                "a%2Bb",
                "a+b",
                "client-key",
            ]
        );
        assert_eq!(format!("{secrets:?}"), "Secrets(18)");
    }

    // Not upstream's: a credential's own secrets are its key, its tokens,
    // its credential headers and its proxy's password, never anything else
    // its metadata keeps.
    #[test]
    fn gathers_a_credentials_secrets() {
        let mut auth = Auth::default();
        auth.attributes
            .insert("api_key".to_owned(), "attribute-key".to_owned());
        auth.attributes.insert(
            "header:Authorization".to_owned(),
            "Bearer header-token".to_owned(),
        );
        auth.attributes
            .insert("header:X-Team".to_owned(), "blue".to_owned());
        auth.proxy_url = "http://proxy-user:proxy-password@127.0.0.1:1".to_owned();
        auth.metadata = json!({
            "access_token": "access-token",
            "refreshToken": "refresh-token",
            "token": {"id_token": "nested-id-token"},
            "email": "someone@example.test",
            "claude_device_ids": {"x": "never-read"},
        })
        .as_object()
        .cloned()
        .unwrap_or_default();
        let mut secrets = Secrets::new();
        secrets.add_auth(&auth);
        assert_eq!(
            secrets.iter().collect::<Vec<_>>(),
            [
                "attribute-key",
                "header-token",
                "proxy-password",
                "cHJveHktdXNlcjpwcm94eS1wYXNzd29yZA==",
                "access-token",
                "refresh-token",
                "nested-id-token",
            ]
        );
    }

    // Not upstream's: a proxy setting gives its URL's password and the
    // `Proxy-Authorization` credential made of it, and none for `direct`.
    #[test]
    fn gathers_a_proxys_password() {
        let mut secrets = Secrets::new();
        secrets.add_proxy("direct");
        secrets.add_proxy(" none ");
        assert!(secrets.is_empty());
        secrets.add_proxy("http://user:proxy-secret-5678@127.0.0.1:3128");
        secrets.add_proxy("http://only-a-user-0123@127.0.0.1:3128");
        assert_eq!(
            secrets.iter().collect::<Vec<_>>(),
            [
                "proxy-secret-5678",
                "dXNlcjpwcm94eS1zZWNyZXQtNTY3OA==",
                "only-a-user-0123",
                "b25seS1hLXVzZXItMDEyMzo=",
            ]
        );
    }
}
