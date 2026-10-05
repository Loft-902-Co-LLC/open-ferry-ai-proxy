// Ported from CLIProxyAPI internal/auth/vertex/keyutil.go
// (NormalizeServiceAccountMap, sanitizePrivateKey, ensureRSAPrivateKey,
// rebuildPEM, filterBase64, stripANSIEscape) and
// internal/runtime/executor/gemini_vertex_executor.go (vertexAccessToken)
// (v8.0.15, MIT), with the service-account token exchange of
// golang.org/x/oauth2 v0.30.0 (google.CredentialsFromJSON, jwt.Config,
// jws.Encode, internal.ParseKey) and Go's encoding/pem (Decode)
// (BSD-3-Clause).
// https://github.com/router-for-me/CLIProxyAPI
// https://cs.opensource.google/go/x/oauth2

//! Access tokens for a Vertex AI service account.
//!
//! The account's `private_key` is cleaned up first: line endings become
//! `\n`, terminal escape sequences go, and a key whose PEM framing was
//! mangled is rebuilt from the base64 between its markers. It must be an
//! RSA key, in PKCS #1 or PKCS #8. [`normalize_service_account`] writes
//! the account back with its key cleaned up so, as PKCS #1, for the
//! management API's Vertex AI import.
//!
//! A token comes from the account's `token_uri` (Google's token endpoint by
//! default) for the `cloud-platform` scope, in exchange for a JWT signed
//! with that key (RFC 7523), and is kept until ten seconds before it
//! expires. Tokens are fetched through the credential's proxy, else the
//! global one.
//!
//! Deviations from upstream:
//! - Tokens are cached per service account, key and endpoint, for up to 256
//!   accounts; upstream fetches a new token for every request.
//! - Only `service_account` credentials are taken; upstream's Google
//!   library also takes user, external-account and impersonation
//!   credentials.
//! - Keys must have 2048 to 8192 bits, which is what the signing library
//!   takes. Key errors read as upstream's, except that the reason after
//!   `private_key invalid rsa:` or `private_key invalid pkcs8:` is the
//!   signing library's rather than Go's.
//! - The fields of the account are matched by their exact names; Go's
//!   decoder ignores case.
//! - The token endpoint's `id_token` isn't read; upstream takes a token's
//!   expiry from it when there is one.
//! - A failed exchange is reported with the endpoint's status and its OAuth
//!   `error` and `error_description`, never its whole body, and with the
//!   assertion's signature redacted where they quote it: the assertion
//!   grants a token until it expires, and the rest of it can be rebuilt.
//!   Upstream logs the description as it came.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aws_lc_rs::rsa::KeyPair;
use aws_lc_rs::signature::RSA_PKCS1_SHA256;
use base64::Engine as _;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::codex::client::{error_chain, read_body_prefix};
use crate::redact;

/// Google's token endpoint, for an account without a `token_uri`.
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// What a Vertex AI token is for.
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// The JWT bearer grant (RFC 7523).
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// How much of the token endpoint's answer is read.
const MAX_TOKEN_BODY: usize = 1 << 20;
/// How long before it expires a token is no longer used.
const EXPIRY_MARGIN: Duration = Duration::from_secs(10);
/// How many accounts' tokens are kept.
const CACHE_LIMIT: usize = 256;

/// Go's `base64.StdEncoding`: padded, but lenient about the unused bits of
/// the last character.
const PEM_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
);

/// A service account whose key is usable. It has no `Debug`, so its key
/// can't be printed.
pub(crate) struct ServiceAccount {
    fields: Map<String, Value>,
    key: KeyPair,
    /// The key as DER, for the cache key.
    key_der: Vec<u8>,
}

/// `NormalizeServiceAccountMap`: the account in `fields`, with its
/// `private_key` cleaned up and parsed.
pub(crate) fn service_account(fields: &Map<String, Value>) -> Result<ServiceAccount, String> {
    let (key, key_der) = parse_private_key(private_key_text(fields)?)?;
    Ok(ServiceAccount {
        fields: fields.clone(),
        key,
        key_der,
    })
}

/// `NormalizeServiceAccountMap`: a copy of the service account `fields`
/// whose `private_key` is cleaned up as for a token exchange and written
/// again as a PKCS #1 PEM block (`RSA PRIVATE KEY`), as the management
/// API's Vertex AI import saves it. Errors read as upstream's and never
/// quote the key.
///
/// Deviations from upstream: the key's DER is written as it came (from
/// inside the PKCS #8 structure for a PKCS #8 key), where upstream encodes
/// the parsed key again, which gives the same bytes for a key in DER; and
/// the headers of an `RSA PRIVATE KEY` block are dropped, where upstream
/// keeps them.
pub fn normalize_service_account(
    fields: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    let block = private_key_block(private_key_text(fields)?)?;
    rsa_key(&block)?;
    let der = match block.kind.as_str() {
        "RSA PRIVATE KEY" => block.der,
        "PRIVATE KEY" => pkcs8_private_key(&block.der)
            .ok_or_else(|| "private_key invalid pkcs8: no private key".to_owned())?
            .to_vec(),
        // `rsa_key` took it as PKCS #1, else as PKCS #8.
        _ => match pkcs8_private_key(&block.der) {
            Some(key) => key.to_vec(),
            None => block.der,
        },
    };
    let mut normalized = fields.clone();
    normalized.insert(
        "private_key".to_owned(),
        Value::String(pem_encode("RSA PRIVATE KEY", &der)),
    );
    Ok(normalized)
}

/// The account's `private_key`, which must be a string that isn't blank.
fn private_key_text(fields: &Map<String, Value>) -> Result<&str, String> {
    let raw = fields
        .get("private_key")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if raw.trim().is_empty() {
        return Err("service account missing private_key".to_owned());
    }
    Ok(raw)
}

/// `sanitizePrivateKey` and `ensureRSAPrivateKey`: the RSA key in `raw`, and
/// its DER.
fn parse_private_key(raw: &str) -> Result<(KeyPair, Vec<u8>), String> {
    let block = private_key_block(raw)?;
    let key = rsa_key(&block)?;
    Ok((key, block.der))
}

/// `sanitizePrivateKey`: the PEM block in `raw`, cleaned up, or rebuilt
/// from the base64 between its markers.
fn private_key_block(raw: &str) -> Result<Pem, String> {
    let text = raw.replace("\r\n", "\n").replace('\r', "\n");
    let text = strip_ansi_escape(&text);
    let text = text.trim();
    match pem_decode(text.as_bytes()) {
        Some(block) => Ok(block),
        None => rebuild_pem(text).map_err(|error| format!("private_key is not valid pem: {error}")),
    }
}

/// The `privateKey` of a PKCS #8 `PrivateKeyInfo`, if `der` reads as one:
/// for an RSA key, its PKCS #1 `RSAPrivateKey`.
fn pkcs8_private_key(der: &[u8]) -> Option<&[u8]> {
    let (tag, info, _) = der_element(der)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, _version, rest) = der_element(info)?;
    if tag != 0x02 {
        return None;
    }
    let (tag, _algorithm, rest) = der_element(rest)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, key, _) = der_element(rest)?;
    (tag == 0x04).then_some(key)
}

/// Go's `pem.EncodeToMemory` for a block without headers: `der` in lines of
/// 64 base64 characters between the markers, each line ended by `\n`.
fn pem_encode(kind: &str, der: &[u8]) -> String {
    let encoded = PEM_BASE64.encode(der);
    let mut out = format!("-----BEGIN {kind}-----\n");
    for line in encoded.as_bytes().chunks(64) {
        // Base64 is ASCII.
        out.push_str(&String::from_utf8_lossy(line));
        out.push('\n');
    }
    out.push_str("-----END ");
    out.push_str(kind);
    out.push_str("-----\n");
    out
}

/// A decoded PEM block.
struct Pem {
    kind: String,
    der: Vec<u8>,
}

/// `ensureRSAPrivateKey`: the block's RSA key.
fn rsa_key(block: &Pem) -> Result<KeyPair, String> {
    match block.kind.as_str() {
        "RSA PRIVATE KEY" => KeyPair::from_der(&block.der)
            .map_err(|error| format!("private_key invalid rsa: {error}")),
        "PRIVATE KEY" => {
            KeyPair::from_pkcs8(&block.der).map_err(|error| match pkcs8_algorithm(&block.der) {
                Some(algorithm) if algorithm != RSA_ENCRYPTION => {
                    "private_key is not an RSA key".to_owned()
                }
                _ => format!("private_key invalid pkcs8: {error}"),
            })
        }
        _ => KeyPair::from_der(&block.der)
            .or_else(|_| KeyPair::from_pkcs8(&block.der))
            .map_err(|_| "private_key uses unsupported format".to_owned()),
    }
}

/// The DER of the rsaEncryption OID, 1.2.840.113549.1.1.1.
const RSA_ENCRYPTION: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];

/// The algorithm OID of a PKCS #8 `PrivateKeyInfo`, if it reads as one.
fn pkcs8_algorithm(der: &[u8]) -> Option<&[u8]> {
    let (tag, info, _) = der_element(der)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, _version, rest) = der_element(info)?;
    if tag != 0x02 {
        return None;
    }
    let (tag, algorithm, _) = der_element(rest)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, oid, _) = der_element(algorithm)?;
    (tag == 0x06).then_some(oid)
}

/// The tag, content and what follows of the DER element `input` starts
/// with.
fn der_element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, mut rest) = rest.split_first()?;
    let length = if first < 0x80 {
        usize::from(first)
    } else {
        let count = usize::from(first & 0x7F);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let (bytes, after) = rest.split_at(count);
        rest = after;
        bytes
            .iter()
            .fold(0usize, |length, &b| (length << 8) | usize::from(b))
    };
    if rest.len() < length {
        return None;
    }
    let (content, after) = rest.split_at(length);
    Some((tag, content, after))
}

/// `rebuildPEM`: the block between the markers of a key whose PEM framing
/// was mangled, from the base64 characters there.
fn rebuild_pem(raw: &str) -> Result<Pem, String> {
    let kind = if raw.contains("RSA PRIVATE KEY") {
        "RSA PRIVATE KEY"
    } else {
        "PRIVATE KEY"
    };
    let header = format!("-----BEGIN {kind}-----");
    let footer = format!("-----END {kind}-----");
    let (Some(start), Some(end)) = (raw.find(&header), raw.find(&footer)) else {
        return Err("missing pem markers".to_owned());
    };
    if end <= start {
        return Err("missing pem markers".to_owned());
    }
    let body = raw.get(start + header.len()..end).unwrap_or_default();
    let payload: String = body
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        .collect();
    if payload.is_empty() {
        return Err("private_key base64 payload empty".to_owned());
    }
    if let Some(offset) = go_base64_error(payload.as_bytes()) {
        return Err(format!(
            "private_key base64 decode failed: illegal base64 data at input byte {offset}"
        ));
    }
    let der = PEM_BASE64
        .decode(payload)
        .map_err(|_| "private_key base64 decode failed: illegal base64 data".to_owned())?;
    Ok(Pem {
        kind: kind.to_owned(),
        der,
    })
}

/// Where Go's `base64.StdEncoding` finds `data` corrupt, if it does, for
/// `data` made of base64 characters and `=` only. Go names that offset in
/// its error, and no character of the key.
fn go_base64_error(data: &[u8]) -> Option<usize> {
    let mut at = 0;
    while at < data.len() {
        // One quantum of up to four characters.
        let mut symbols = 0;
        while symbols < 4 {
            if at == data.len() {
                return Some(at - symbols);
            }
            let byte = data[at];
            at += 1;
            if byte != b'=' {
                symbols += 1;
                continue;
            }
            match symbols {
                0 | 1 => return Some(at - 1),
                2 => {
                    // A second `=` must follow.
                    if at == data.len() {
                        return Some(at);
                    }
                    if data[at] != b'=' {
                        return Some(at - 1);
                    }
                    at += 1;
                }
                _ => {}
            }
            // Nothing may follow the padding.
            return (at < data.len()).then_some(at);
        }
    }
    None
}

/// `stripANSIEscape`: drops terminal escape sequences: `ESC ]` up to a BEL
/// or `ESC \`, `ESC [` up to its final letter, and any other `ESC`.
fn strip_ansi_escape(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\x1b' {
            out.push(c);
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some(']') => {
                i += 2;
                while i < chars.len() {
                    if chars[i] == '\x07' {
                        break;
                    }
                    if chars[i] == '\x1b' && chars.get(i + 1) == Some(&'\\') {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            Some('[') => {
                i += 2;
                while i < chars.len() && !chars[i].is_ascii_alphabetic() {
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Go's `pem.Decode` (Go 1.26): the first PEM block in `data`, headers
/// skipped. It looks for the first END line, then the last BEGIN line
/// before it.
fn pem_decode(data: &[u8]) -> Option<Pem> {
    const START: &[u8] = b"\n-----BEGIN ";
    const END: &[u8] = b"\n-----END ";
    const END_OF_LINE: &[u8] = b"-----";

    let mut rest = data;
    let mut end_trailer_index: isize = 0;
    loop {
        // Past the END line of the block that failed.
        rest = rest.get(usize::try_from(end_trailer_index).ok()?..)?;

        let end_at = find(rest, END)?;
        end_trailer_index = signed(end_at + END.len());
        let Some(begin_at) = rfind(&rest[..end_at], &START[1..]) else {
            continue;
        };
        if begin_at > 0 && rest[begin_at - 1] != b'\n' {
            continue;
        }
        let skip = begin_at + START.len() - 1;
        rest = &rest[skip..];
        let mut end_index = signed(end_at) - signed(skip);
        end_trailer_index -= signed(skip);

        let (type_line, next, consumed) = get_line(rest);
        rest = next;
        end_index -= signed(consumed);
        end_trailer_index -= signed(consumed);
        let Some(kind) = type_line.strip_suffix(END_OF_LINE) else {
            continue;
        };

        let mut has_headers = false;
        loop {
            if rest.is_empty() {
                return None;
            }
            let (line, next, consumed) = get_line(rest);
            if !line.contains(&b':') {
                break;
            }
            has_headers = true;
            rest = next;
            end_index -= signed(consumed);
            end_trailer_index -= signed(consumed);
        }
        // Headers must end before the END line.
        if has_headers && end_index < 0 {
            continue;
        }

        // The END line names the same type and ends in dashes.
        let Some(trailer) = usize::try_from(end_trailer_index)
            .ok()
            .and_then(|at| rest.get(at..))
        else {
            continue;
        };
        let trailer_len = kind.len() + END_OF_LINE.len();
        if trailer.len() < trailer_len {
            continue;
        }
        let (end_line, rest_of_line) = trailer.split_at(trailer_len);
        if !end_line.starts_with(kind) || !end_line.ends_with(END_OF_LINE) {
            continue;
        }
        if !get_line(rest_of_line).0.is_empty() {
            continue;
        }

        let der = match usize::try_from(end_index) {
            Ok(end_index) if end_index > 0 => {
                // Go's decoder skips line breaks; spaces and tabs go first.
                let base64: Vec<u8> = rest[..end_index]
                    .iter()
                    .copied()
                    .filter(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
                    .collect();
                let Ok(der) = PEM_BASE64.decode(base64) else {
                    continue;
                };
                der
            }
            _ => Vec::new(),
        };
        return Some(Pem {
            kind: String::from_utf8_lossy(kind).into_owned(),
            der,
        });
    }
}

/// `n` as a signed offset. Slices are never longer than `isize::MAX`.
fn signed(n: usize) -> isize {
    isize::try_from(n).unwrap_or(isize::MAX)
}

/// Go's `pem.getLine`: the first line without its ending and trailing
/// spaces and tabs, the rest, and how many bytes the line took.
fn get_line(data: &[u8]) -> (&[u8], &[u8], usize) {
    let (mut line, rest, consumed) = match data.iter().position(|&b| b == b'\n') {
        Some(at) => (&data[..at], &data[at + 1..], at + 1),
        None => (data, &data[data.len()..], data.len()),
    };
    line = line.strip_suffix(b"\r").unwrap_or(line);
    while let Some((&last, init)) = line.split_last() {
        if last != b' ' && last != b'\t' {
            break;
        }
        line = init;
    }
    (line, rest, consumed)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

/// What a token exchange needs, read from the account
/// (`google.CredentialsFromJSON`).
struct Exchange<'a> {
    client_email: &'a str,
    private_key_id: &'a str,
    token_uri: &'a str,
    audience: &'a str,
    key: &'a KeyPair,
}

impl ServiceAccount {
    /// The string field `name`, empty when it is missing or `null`.
    fn string(&self, name: &str) -> Result<&str, String> {
        match self.fields.get(name) {
            None | Some(Value::Null) => Ok(""),
            Some(Value::String(text)) => Ok(text),
            Some(_) => Err(format!("field {name} is not a string")),
        }
    }

    fn exchange(&self) -> Result<Exchange<'_>, String> {
        match self.string("type")? {
            "service_account" => {}
            "" => return Err("missing 'type' field in credentials".to_owned()),
            other => {
                return Err(format!(
                    "unknown credential type: {}",
                    open_ferry_translate::go::quote(other)
                ));
            }
        }
        let token_uri = match self.string("token_uri")? {
            "" => DEFAULT_TOKEN_URI,
            uri => uri,
        };
        Ok(Exchange {
            client_email: self.string("client_email")?,
            private_key_id: self.string("private_key_id")?,
            token_uri,
            audience: self.string("audience")?,
            key: &self.key,
        })
    }
}

impl Exchange<'_> {
    /// What the cached token is kept under: a hash of the account, its key
    /// and the endpoint.
    fn cache_key(&self, key_der: &[u8]) -> [u8; 32] {
        let mut hash = Sha256::new();
        for part in [
            self.client_email.as_bytes(),
            self.private_key_id.as_bytes(),
            self.token_uri.as_bytes(),
            self.audience.as_bytes(),
            key_der,
        ] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part);
        }
        hash.finalize().into()
    }

    /// The signed JWT asserting the account (`jws.Encode`), issued ten
    /// seconds ago, as Go's library does for clocks running ahead, and
    /// valid for an hour.
    fn assertion(&self, now: u64) -> Result<String, String> {
        let issued = now.saturating_sub(10);
        let mut header = json!({"alg": "RS256", "typ": "JWT"});
        if !self.private_key_id.is_empty() {
            header["kid"] = Value::from(self.private_key_id);
        }
        let audience = if self.audience.is_empty() {
            self.token_uri
        } else {
            self.audience
        };
        let claims = json!({
            "iss": self.client_email,
            "scope": SCOPE,
            "aud": audience,
            "exp": issued + 3600,
            "iat": issued,
        });
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &aws_lc_rs::rand::SystemRandom::new(),
                signed.as_bytes(),
                &mut signature,
            )
            .map_err(|_| "jws: signing failed".to_owned())?;
        Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }

    /// Exchanges the JWT for a token (`jwtSource.Token`).
    async fn fetch(&self, client: &reqwest::Client) -> Result<Fetched, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let assertion = self.assertion(now)?;
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("assertion", &assertion)
            .append_pair("grant_type", GRANT_TYPE)
            .finish();
        let response = client
            .post(self.token_uri)
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form)
            .send()
            .await
            .map_err(|error| {
                format!(
                    "oauth2: cannot fetch token: {}",
                    error_chain(&error.without_url())
                )
            })?;
        let status = response.status();
        let (body, read_error) = read_body_prefix(response, MAX_TOKEN_BODY).await;
        if let Some(error) = read_error {
            return Err(format!(
                "oauth2: cannot fetch token: {}",
                error_chain(&error.without_url())
            ));
        }
        if !status.is_success() {
            let signature = assertion.rsplit('.').next().unwrap_or_default();
            return Err(format!(
                "oauth2: cannot fetch token: {status}{}",
                redact::text(oauth_error(&body), signature)
            ));
        }
        let answer: TokenAnswer = serde_json::from_slice(&body)
            .map_err(|_| "oauth2: cannot fetch token: the answer isn't a token".to_owned())?;
        Ok(Fetched {
            access_token: answer.access_token.unwrap_or_default(),
            expires_in: answer.expires_in.unwrap_or_default(),
        })
    }
}

/// The OAuth `error` and `error_description` of a failed exchange, if it
/// has them.
fn oauth_error(body: &[u8]) -> String {
    let Ok(Value::Object(answer)) = serde_json::from_slice::<Value>(body) else {
        return String::new();
    };
    let field = |name: &str| answer.get(name).and_then(Value::as_str).unwrap_or_default();
    match (field("error"), field("error_description")) {
        ("", _) => String::new(),
        (error, "") => format!(" ({error})"),
        (error, description) => format!(" ({error}: {description})"),
    }
}

/// The token endpoint's answer.
#[derive(Deserialize)]
struct TokenAnswer {
    access_token: Option<String>,
    expires_in: Option<i64>,
}

/// A token as fetched.
struct Fetched {
    access_token: String,
    /// Seconds until it expires, or 0 when the endpoint didn't say.
    expires_in: i64,
}

/// A cached token. It has no `Debug`.
struct Cached {
    access_token: String,
    expires_at: Instant,
}

impl Cached {
    fn is_fresh(&self) -> bool {
        Instant::now() + EXPIRY_MARGIN < self.expires_at
    }
}

type Slot = Arc<tokio::sync::Mutex<Option<Cached>>>;

/// Tokens by account, each fetched once at a time. It has no `Debug`.
#[derive(Default)]
pub(crate) struct TokenCache {
    slots: Mutex<HashMap<[u8; 32], Slot>>,
}

impl TokenCache {
    /// `vertexAccessToken`: a token for `account`, cached or fetched with
    /// `client`. It may be empty when the endpoint gave none.
    pub(crate) async fn token(
        &self,
        client: &reqwest::Client,
        account: &ServiceAccount,
    ) -> Result<String, String> {
        let exchange = account.exchange().map_err(|error| {
            format!("vertex executor: parse service account json failed: {error}")
        })?;
        let slot = self.slot(exchange.cache_key(&account.key_der));
        let mut cached = slot.lock().await;
        if let Some(token) = cached.as_ref().filter(|token| token.is_fresh()) {
            return Ok(token.access_token.clone());
        }
        let fetched = exchange
            .fetch(client)
            .await
            .map_err(|error| format!("vertex executor: get access token failed: {error}"))?;
        *cached = u64::try_from(fetched.expires_in)
            .ok()
            .filter(|&seconds| seconds > 0 && !fetched.access_token.is_empty())
            .map(|seconds| Cached {
                access_token: fetched.access_token.clone(),
                expires_at: Instant::now() + Duration::from_secs(seconds),
            });
        Ok(fetched.access_token)
    }

    /// The slot for `key`, making room by dropping slots not in use whose
    /// tokens are stale, or failing that all slots not in use.
    fn slot(&self, key: [u8; 32]) -> Slot {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if !slots.contains_key(&key) && slots.len() >= CACHE_LIMIT {
            slots.retain(|_, slot| {
                Arc::strong_count(slot) > 1
                    || slot
                        .try_lock()
                        .is_ok_and(|token| token.as_ref().is_some_and(Cached::is_fresh))
            });
            if slots.len() >= CACHE_LIMIT {
                slots.retain(|_, slot| Arc::strong_count(slot) > 1);
            }
        }
        Arc::clone(slots.entry(key).or_default())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests;
