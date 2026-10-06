// Ported from CLIProxyAPI internal/util/provider.go (HideAPIKey,
// MaskAuthorizationHeader, MaskSensitiveHeaderValue, MaskSensitiveQuery,
// shouldMaskQueryParam) and internal/logging/diagnostic.go
// (SafeDiagnosticForLog, SafeErrorDiagnostic, diagnosticRunePrefix,
// truncateDiagnosticLogExcerpt) (v8.0.15, MIT), and Go's net/url/url.go
// (QueryUnescape, PathUnescape, QueryEscape, shouldEscape) (go1.26,
// BSD-3-Clause).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/golang/go

//! Masking for what the logs and the usage records keep: an API key cut to
//! its ends ([`hide_api_key`]), credential headers and query parameters
//! masked ([`mask_header_value`], [`mask_sensitive_query`]), an error's
//! text bounded and scrubbed for an ordinary log line
//! ([`safe_diagnostic_for_log`], [`safe_error_diagnostic`]), and email
//! addresses masked ([`mask_emails`]).
//!
//! Deviations from upstream:
//! - [`mask_emails`] isn't upstream's: upstream logs an account's email,
//!   and the names of the auth files that hold one, as they are.
//! - A key cut through a multibyte character shows U+FFFD in place of the
//!   broken bytes; [`hide_bytes`] keeps them, as Go does.
//! - [`is_credential_header`] and [`mask_header_value`] also mask `Cookie`,
//!   `Set-Cookie`, `X-Management-Key` and `X-Local-Password`, which
//!   upstream's request log writes in clear.
//! - [`mask_header_value`] and [`mask_sensitive_query`] hide a credential
//!   of one or two bytes whole, as `...`, where upstream keeps it as it is
//!   ([`hide_log_bytes`]).
//! - [`mask_sensitive_query`] also hides the values of `code`, `state`,
//!   `auth` and `sig`, and of any parameter whose name holds `password`,
//!   `passwd`, `credential`, `authorization`, `signature` or `verifier`
//!   ([`is_secret_query_param`]). Upstream's masks only key-like names, so
//!   its access line writes an OAuth callback's code and state as they
//!   came.
//! - The patterns are upstream's with Go's ASCII-only `\s` and `\b` written
//!   out, since Rust's are Unicode-aware.
//! - [`safe_error_diagnostic`] looks for its signals in the text of the
//!   whole source chain, joined as Go joins a wrapped error's, since a Rust
//!   error's text usually leaves out its source's. Go's `io.EOF` is an
//!   error whose text is `EOF`, an `io::Error` of kind `UnexpectedEof` is
//!   `unexpected_EOF` and one of kind `TimedOut` a timeout. There is no
//!   `context.Canceled`, so no `canceled`, and the fallback names the Rust
//!   type.

use std::borrow::Cow;
use std::error::Error;
use std::io;
use std::sync::LazyLock;

use open_ferry_translate::go::{equal_fold, to_lower, trim_space};
use regex::Regex;

/// How many characters of a diagnostic a log line keeps
/// (`diagnosticLogRuneLimit`).
const DIAGNOSTIC_LIMIT: usize = 300;

/// How many characters of a diagnostic are read
/// (`diagnosticLogScanRuneLimit`).
const DIAGNOSTIC_SCAN_LIMIT: usize = 600;

/// What a cut diagnostic puts before the access-token-expired signal it
/// keeps.
const SEPARATOR: &str = " ... ";

/// The parts of a header name upstream masks the header for
/// (`MaskSensitiveHeaderValue`).
const CREDENTIAL_NAME_PARTS: [&str; 5] = ["authorization", "api-key", "apikey", "token", "secret"];

/// The credential headers upstream's request log writes in clear.
const OTHER_CREDENTIAL_HEADERS: [&str; 5] = [
    "cookie",
    "set-cookie",
    "proxy-authorization",
    "x-management-key",
    "x-local-password",
];

/// A Go pattern as Rust reads it: Go's `\s` and `\b` are ASCII-only.
fn go_regex(pattern: &str) -> Regex {
    // The patterns are constants; a bad one fails every test.
    Regex::new(
        &pattern
            .replace("\\s", "[\\t\\n\\f\\r ]")
            .replace("\\b", "(?-u:\\b)"),
    )
    .unwrap_or_else(|_| Regex::new("$^").unwrap_or_else(|_| unreachable!()))
}

static ACCESS_TOKEN_EXPIRED: LazyLock<Regex> =
    LazyLock::new(|| go_regex(r"(?i)access token expired"));
static SENSITIVE_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    go_regex(concat!(
        r#"(?i)(["']?(?:access[\s_-]*token|refresh[\s_-]*token|id[\s_-]*token|api[\s_-]*key|client[\s_-]*secret|private[\s_-]*key|proxy[\s_-]*authorization|authorization|password|credential|token|secret)["']?\s*[:=]\s*)"#,
        r#"(?:(?:bearer|basic)\s+[^\s,;]+|"(?:\\.|[^"])*"|'(?:\\.|[^'])*'|[^\s,;&}\]]+)"#,
    ))
});
static AUTHORIZATION: LazyLock<Regex> =
    LazyLock::new(|| go_regex(r"(?i)\b(bearer|basic)\s+[^\s,;]+"));
static URL_USERINFO: LazyLock<Regex> =
    LazyLock::new(|| go_regex(r"(?i)([a-z][a-z0-9+.-]*://)[^/\s@]+@"));
static STATUS: LazyLock<Regex> =
    LazyLock::new(|| go_regex(r"(?i)\bstatus(?:\s+code)?\s*[:=]?\s*([1-5][0-9]{2})\b"));

/// `key` cut to its ends, as upstream's `HideAPIKey` cuts it: the first and
/// last four bytes of a key longer than eight, two of one longer than four,
/// one of one longer than two, and a shorter key as it is.
pub fn hide_bytes(key: &[u8]) -> Vec<u8> {
    let keep = match key.len() {
        len if len > 8 => 4,
        len if len > 4 => 2,
        len if len > 2 => 1,
        _ => return key.to_vec(),
    };
    let head = key.get(..keep).unwrap_or_default();
    let tail = key
        .get(key.len().saturating_sub(keep)..)
        .unwrap_or_default();
    let mut out = Vec::with_capacity(keep * 2 + 3);
    out.extend_from_slice(head);
    out.extend_from_slice(b"...");
    out.extend_from_slice(tail);
    out
}

/// Upstream's `HideAPIKey`: [`hide_bytes`] as text.
pub fn hide_api_key(key: &str) -> String {
    String::from_utf8_lossy(&hide_bytes(key.as_bytes())).into_owned()
}

/// [`hide_bytes`] for what a log keeps: a key of one or two bytes, which
/// [`hide_bytes`] keeps as it is, is hidden whole as `...`.
pub fn hide_log_bytes(key: &[u8]) -> Vec<u8> {
    match key.len() {
        0 => Vec::new(),
        1 | 2 => b"...".to_vec(),
        _ => hide_bytes(key),
    }
}

/// [`hide_log_bytes`] as text.
pub fn hide_log_key(key: &str) -> String {
    String::from_utf8_lossy(&hide_log_bytes(key.as_bytes())).into_owned()
}

/// Upstream's `MaskAuthorizationHeader`: an `Authorization` value with its
/// scheme kept and its credential hidden, or all of it hidden when it has
/// no scheme.
pub fn mask_authorization_header(value: &str) -> String {
    match value.trim().split_once(' ') {
        Some((scheme, credential)) => format!("{scheme} {}", hide_api_key(credential)),
        None => hide_api_key(value),
    }
}

/// Upstream's `MaskSensitiveHeaderValue`: the value of header `key`, masked
/// when the name holds `authorization`, `api-key`, `apikey`, `token` or
/// `secret`.
pub fn mask_sensitive_header_value(key: &str, value: &str) -> String {
    let lower = to_lower(key.trim());
    if lower.contains("authorization") {
        mask_authorization_header(value)
    } else if CREDENTIAL_NAME_PARTS
        .iter()
        .any(|part| lower.contains(part))
    {
        hide_api_key(value)
    } else {
        value.to_owned()
    }
}

/// Whether header `name` carries a credential: upstream's sensitive names,
/// and the cookies, `Proxy-Authorization`, `X-Management-Key` and
/// `X-Local-Password`.
pub fn is_credential_header(name: &str) -> bool {
    let lower = to_lower(name.trim());
    CREDENTIAL_NAME_PARTS
        .iter()
        .any(|part| lower.contains(part))
        || OTHER_CREDENTIAL_HEADERS.contains(&lower.as_str())
}

/// The value of header `name` as a log may keep it: masked as upstream
/// masks it ([`mask_sensitive_header_value`]), and for the other credential
/// headers ([`is_credential_header`]) as upstream masks an API key; but a
/// credential of one or two bytes is hidden whole ([`hide_log_bytes`]).
pub fn mask_header_value(name: &str, value: &str) -> String {
    let lower = to_lower(name.trim());
    if lower.contains("authorization") {
        match value.trim().split_once(' ') {
            Some((scheme, credential)) => format!("{scheme} {}", hide_log_key(credential)),
            None => hide_log_key(value),
        }
    } else if is_credential_header(name) {
        hide_log_key(value)
    } else {
        value.to_owned()
    }
}

/// Upstream's `MaskSensitiveQuery`: a raw query with the value of every
/// parameter that holds a secret hidden ([`is_secret_query_param`]), its
/// name kept, or the query as it is when it has none. A value of one or
/// two bytes, which upstream keeps, is hidden whole ([`hide_log_bytes`]).
pub fn mask_sensitive_query(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut changed = false;
    let parts: Vec<String> = raw
        .split('&')
        .map(|part| {
            if part.is_empty() {
                return String::new();
            }
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            let decoded_key = query_unescape(key)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_else(|| key.to_owned());
            if !is_secret_query_param(&decoded_key) {
                return part.to_owned();
            }
            let decoded_value = query_unescape(value).unwrap_or_else(|| value.as_bytes().to_vec());
            changed = true;
            format!(
                "{key}={}",
                query_escape(&hide_log_bytes(trim_space(&decoded_value)))
            )
        })
        .collect();
    if changed {
        parts.join("&")
    } else {
        raw.to_owned()
    }
}

/// Upstream's `shouldMaskQueryParam`: whether a query parameter named
/// `key` holds a key, a token or a secret.
pub(crate) fn should_mask_query_param(key: &str) -> bool {
    let key = to_lower(key.trim());
    if key.is_empty() {
        return false;
    }
    let key = key.strip_suffix("[]").unwrap_or(&key);
    key == "key"
        || ["api-key", "apikey", "api_key", "token", "secret"]
            .iter()
            .any(|part| key.contains(part))
}

/// The query parameter names, beyond upstream's, whose values a log hides:
/// an OAuth callback's code and state, and other credentials.
const SECRET_QUERY_NAMES: [&str; 4] = ["code", "state", "auth", "sig"];

/// The parts of a query parameter name, beyond upstream's, that make its
/// value one a log hides: passwords, a signed URL's credential and
/// signature, and a PKCE verifier.
const SECRET_QUERY_NAME_PARTS: [&str; 6] = [
    "password",
    "passwd",
    "credential",
    "authorization",
    "signature",
    "verifier",
];

/// Whether a query parameter named `key` holds a secret a log mustn't
/// show: upstream's key-like names (its `shouldMaskQueryParam`); `code`,
/// `state`, `auth` and `sig`; and any name holding `password`, `passwd`,
/// `credential`, `authorization`, `signature` or `verifier`. Case and a
/// trailing `[]` don't count.
pub fn is_secret_query_param(key: &str) -> bool {
    if should_mask_query_param(key) {
        return true;
    }
    let key = to_lower(key.trim());
    let key = key.strip_suffix("[]").unwrap_or(&key);
    SECRET_QUERY_NAMES.contains(&key)
        || SECRET_QUERY_NAME_PARTS
            .iter()
            .any(|part| key.contains(part))
}

/// Go's `url.QueryUnescape`, or `None` where it fails.
pub(crate) fn query_unescape(text: &str) -> Option<Vec<u8>> {
    unescape(text, true)
}

/// Go's `url.PathUnescape`, or `None` where it fails: as
/// [`query_unescape`], but `+` is kept.
pub(crate) fn percent_unescape(text: &str) -> Option<Vec<u8>> {
    unescape(text, false)
}

/// `text` with its `%XX` escapes decoded, and `+` read as a space when
/// `plus_is_space`, or `None` for a broken escape.
fn unescape(text: &str, plus_is_space: bool) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'%' => {
                let high = hex(bytes.next()?)?;
                let low = hex(bytes.next()?)?;
                out.push(high << 4 | low);
            }
            b'+' if plus_is_space => out.push(b' '),
            _ => out.push(byte),
        }
    }
    Some(out)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Go's `url.QueryEscape`.
fn query_escape(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let digit = |nibble: u8| char::from(HEX.get(usize::from(nibble)).copied().unwrap_or(b'0'));
    let mut out = String::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(digit(byte >> 4));
                out.push(digit(byte & 0x0f));
            }
        }
    }
    out
}

/// What stands for the hidden part of an email's local part and domain.
const HIDDEN_EMAIL_PART: &str = "***";

/// The providers whose auth files are named `<provider>-<email>…json`, or
/// `<provider>-<hash>-<email>…json`; [`mask_emails`] keeps the prefix.
const CREDENTIAL_FILE_PREFIXES: [&str; 5] = ["antigravity", "claude", "codex", "gemini", "xai"];

/// The file extensions [`mask_emails`] doesn't take for a domain's last
/// label: an auth file's, a saved cooldown's and a temporary file's.
const FILE_EXTENSIONS: [&str; 3] = ["json", "cds", "tmp"];

/// `text` with every email address in it masked, to tell accounts apart
/// without naming them: the first character of the local part, the first
/// of the domain and the domain's last label, so `john.doe@example.com`
/// becomes `j***@e***.com`.
///
/// An address is found wherever it stands: in a sentence, a file name, a
/// Windows or Unix path, quotes, or a query, where its `@` may be written
/// `%40` (kept so). An auth file's name keeps its provider prefix and
/// hash, and its extension and plan: `claude-john@example.com.json`
/// becomes `claude-j***@e***.com.json`, and
/// `codex-1a2b3c4d-john@example.com-plus.json`
/// `codex-1a2b3c4d-j***@e***.com-plus.json`.
///
/// An address needs a local part, a domain of two labels or more, and a
/// last label of two letters or more, so `user@localhost`,
/// `claude-3-5-sonnet@20240620`, `pkg@1.2.3`, `@scope/pkg` and `a@b.c`
/// aren't masked. Masking is deterministic and idempotent, and costs one
/// scan of a text without `@` or `%`, which is returned as it is.
pub fn mask_emails(text: &str) -> Cow<'_, str> {
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut from = 0;
    // A local part starts after the at sign before it: `%40` is made of
    // characters a local part may hold, so without this each `%40` of a
    // long run would be scanned back to the run's start.
    let mut floor = 0;
    while let Some((at, after)) = next_at_sign(text, from) {
        from = after;
        let lowest = copied.max(floor);
        floor = after;
        let Some(email) = email_at(text, lowest, at, after) else {
            continue;
        };
        let out = out.get_or_insert_with(|| String::with_capacity(text.len()));
        out.push_str(text.get(copied..email.start).unwrap_or_default());
        out.push_str(email.kept);
        out.push(email.local_first);
        out.push_str(HIDDEN_EMAIL_PART);
        out.push_str(text.get(at..after).unwrap_or_default());
        out.push(email.domain_first);
        out.push_str(HIDDEN_EMAIL_PART);
        out.push('.');
        out.push_str(email.last_label);
        copied = email.end;
        from = email.end;
    }
    match out {
        None => Cow::Borrowed(text),
        Some(mut out) => {
            out.push_str(text.get(copied..).unwrap_or_default());
            Cow::Owned(out)
        }
    }
}

/// An email address found in a text.
struct Email<'a> {
    /// Where it starts.
    start: usize,
    /// Where it ends: after its last label.
    end: usize,
    /// The auth file prefix it keeps, such as `claude-`.
    kept: &'a str,
    /// The first character of its local part.
    local_first: char,
    /// The first character of its domain.
    domain_first: char,
    /// Its domain's last label.
    last_label: &'a str,
}

/// The next `@`, or `%40`, in `text` at or after byte `from`: where it
/// starts and where it ends.
fn next_at_sign(text: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut index = from;
    loop {
        let found = index
            + bytes
                .get(index..)?
                .iter()
                .position(|&b| b == b'@' || b == b'%')?;
        if bytes.get(found) == Some(&b'@') {
            return Some((found, found + 1));
        }
        if bytes.get(found + 1..found + 3) == Some(b"40".as_slice()) {
            return Some((found, found + 3));
        }
        index = found + 1;
    }
}

/// Whether `c` can be in an email's local part as a log shows it: `*`
/// isn't, so a masked address isn't masked again.
fn is_local_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

/// Whether `c` can be in an email's domain.
fn is_domain_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '-')
}

/// The email address around the at sign from byte `at` to `after` in
/// `text`, its local part starting at byte `floor` or later, if there is
/// one.
fn email_at(text: &str, floor: usize, at: usize, after: usize) -> Option<Email<'_>> {
    let before = text.get(floor..at)?;
    let local_len: usize = before
        .chars()
        .rev()
        .take_while(|&c| is_local_char(c))
        .map(char::len_utf8)
        .sum();
    let local = before
        .get(before.len().saturating_sub(local_len)..)?
        .trim_start_matches(['.', '-']);
    let start = at.saturating_sub(local.len());
    let (kept, local) = local.split_at_checked(credential_prefix_len(local))?;
    let local_first = local.chars().next()?;

    let rest = text.get(after..)?;
    let domain_len: usize = rest
        .chars()
        .take_while(|&c| is_domain_char(c))
        .map(char::len_utf8)
        .sum();
    let mut labels = Vec::new();
    let mut offset = 0;
    for label in rest.get(..domain_len)?.split('.') {
        if label.is_empty() {
            break;
        }
        labels.push((offset, label));
        offset += label.len() + 1;
    }
    let domain_first = labels
        .first()?
        .1
        .chars()
        .next()
        .filter(|c| c.is_alphanumeric())?;
    let (offset, last_label) =
        labels
            .iter()
            .enumerate()
            .skip(1)
            .rev()
            .find_map(|(index, &(offset, label))| {
                if index >= 2
                    && FILE_EXTENSIONS
                        .iter()
                        .any(|extension| label.eq_ignore_ascii_case(extension))
                {
                    return None;
                }
                let head = label.split('-').next().unwrap_or(label);
                (head.chars().nth(1).is_some() && head.chars().all(char::is_alphabetic))
                    .then_some((offset, head))
            })?;
    Some(Email {
        start,
        end: after + offset + last_label.len(),
        kept,
        local_first,
        domain_first,
        last_label,
    })
}

/// How much of the local part `local` is an auth file's prefix: a
/// provider of [`CREDENTIAL_FILE_PREFIXES`] and `-`, then maybe a hash of
/// eight lowercase hex digits and `-`; none when nothing would be left.
fn credential_prefix_len(local: &str) -> usize {
    let Some(rest) = CREDENTIAL_FILE_PREFIXES
        .iter()
        .find_map(|provider| local.strip_prefix(provider)?.strip_prefix('-'))
    else {
        return 0;
    };
    let rest = match rest
        .split_at_checked(8)
        .filter(|(hash, _)| {
            hash.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .and_then(|(_, after)| after.strip_prefix('-'))
    {
        Some(after) if !after.is_empty() => after,
        _ => rest,
    };
    if rest.is_empty() {
        0
    } else {
        local.len().saturating_sub(rest.len())
    }
}

/// Upstream's `SafeDiagnosticForLog`: `message` on one line, at most 300
/// characters and an ellipsis, with URL user info, credential assignments
/// and `Bearer` or `Basic` credentials redacted. An "access token expired"
/// in it is kept, even past the cut.
pub fn safe_diagnostic_for_log(message: &str) -> String {
    let (prefix, source_truncated) = rune_prefix(message, DIAGNOSTIC_SCAN_LIMIT);
    let mut excerpt = prefix.to_owned();
    if source_truncated
        && !ACCESS_TOKEN_EXPIRED.is_match(&excerpt)
        && let Some(marker) = ACCESS_TOKEN_EXPIRED.find(message)
    {
        excerpt.push_str(SEPARATOR);
        excerpt.push_str(marker.as_str());
    }

    let excerpt = excerpt.split_whitespace().collect::<Vec<_>>().join(" ");
    if excerpt.is_empty() {
        return String::new();
    }
    let excerpt = URL_USERINFO.replace_all(&excerpt, "${1}[REDACTED]@");
    let excerpt = SENSITIVE_ASSIGNMENT.replace_all(&excerpt, "${1}\"[REDACTED]\"");
    let excerpt = AUTHORIZATION.replace_all(&excerpt, "${1} [REDACTED]");

    truncate_excerpt(&excerpt, source_truncated)
}

/// Upstream's `SafeErrorDiagnostic`: only the known failure signals in
/// `error`, such as `connection_refused` or `status=401`, never its text;
/// with none, the error's type.
pub fn safe_error_diagnostic<E: Error + 'static>(error: &E) -> String {
    let chain: Vec<&(dyn Error + 'static)> =
        std::iter::successors(Some(error as &(dyn Error + 'static)), |&error| {
            error.source()
        })
        .collect();
    let io_kind = |kind: io::ErrorKind| {
        chain.iter().any(|error| {
            error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == kind)
        })
    };
    let texts: Vec<String> = chain.iter().map(ToString::to_string).collect();
    let is_eof = |text: &String| equal_fold(text.trim(), "EOF");

    let mut parts: Vec<&str> = Vec::with_capacity(4);
    if io_kind(io::ErrorKind::UnexpectedEof) {
        push_part(&mut parts, "unexpected_EOF");
    } else if texts.iter().any(is_eof) {
        push_part(&mut parts, "EOF");
    }
    if io_kind(io::ErrorKind::TimedOut) {
        push_part(&mut parts, "timeout");
    }
    if texts.first().is_some_and(is_eof) {
        push_part(&mut parts, "EOF");
    }

    let raw_original = texts.join(": ");
    let raw = to_lower(&raw_original);
    if raw.contains("socks")
        && (raw.contains("authentication failed") || raw.contains("authentication required"))
    {
        push_part(&mut parts, "proxy_authentication_failed");
    }
    for (needle, label) in [
        ("socks", "proxy=socks"),
        ("proxyconnect", "proxy_connect_failed"),
        ("proxy connect", "proxy_connect_failed"),
        ("dial ", "dial_failed"),
        ("dial failed", "dial_failed"),
        ("connection refused", "connection_refused"),
        ("connection reset", "connection_reset"),
        ("connection aborted", "connection_aborted"),
        ("stream reset", "stream_reset"),
        ("network is unreachable", "network_unreachable"),
        ("no route to host", "network_unreachable"),
        ("no such host", "dns_not_found"),
        ("server misbehaving", "dns_failure"),
        ("tls handshake timeout", "tls_handshake_timeout"),
        ("i/o timeout", "timeout"),
        ("deadline exceeded", "timeout"),
        ("unexpected eof", "unexpected_EOF"),
        ("certificate", "tls_certificate_error"),
        ("invalid character", "invalid_response_json"),
        ("cannot unmarshal", "invalid_response_json"),
        ("invalid_grant", "oauth_error=invalid_grant"),
        ("refresh_token_expired", "oauth_error=refresh_token_expired"),
        ("refresh_token_revoked", "oauth_error=refresh_token_revoked"),
        ("refresh_token_reused", "oauth_error=refresh_token_reused"),
    ] {
        if raw.contains(needle) {
            push_part(&mut parts, label);
        }
    }
    let mut out = parts.join(" ");
    if let Some(status) = STATUS
        .captures(&raw_original)
        .and_then(|captures| captures.get(1))
    {
        let status = format!("status={}", status.as_str());
        if !parts.contains(&status.as_str()) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&status);
        }
    }
    if out.is_empty() {
        out = format!("error_type={}", std::any::type_name::<E>());
    }
    out
}

/// Adds `part` to `parts` unless it is there already.
fn push_part<'a>(parts: &mut Vec<&'a str>, part: &'a str) {
    if !parts.contains(&part) {
        parts.push(part);
    }
}

/// Upstream's `diagnosticRunePrefix`: the first `limit` characters of
/// `value`, and whether that cut any.
fn rune_prefix(value: &str, limit: usize) -> (&str, bool) {
    match value.char_indices().nth(limit) {
        Some((index, _)) => (value.get(..index).unwrap_or(value), true),
        None => (value, false),
    }
}

/// Upstream's `truncateDiagnosticLogExcerpt`: `message` cut to 300
/// characters and an ellipsis, keeping an "access token expired" past the
/// cut, or with an ellipsis added when the source was cut.
fn truncate_excerpt(message: &str, source_truncated: bool) -> String {
    let runes: Vec<char> = message.chars().collect();
    if runes.len() <= DIAGNOSTIC_LIMIT {
        return if source_truncated {
            format!("{message}...")
        } else {
            message.to_owned()
        };
    }

    let mut output: String = runes.iter().take(DIAGNOSTIC_LIMIT).collect();
    if let Some(marker) = ACCESS_TOKEN_EXPIRED.find(message) {
        let start = message
            .get(..marker.start())
            .map_or(0, |before| before.chars().count());
        let end = start + marker.as_str().chars().count();
        if end > DIAGNOSTIC_LIMIT {
            let prefix_limit =
                DIAGNOSTIC_LIMIT.saturating_sub(SEPARATOR.chars().count() + (end - start));
            output = runes.iter().take(prefix_limit).collect();
            output.push_str(SEPARATOR);
            output.extend(runes.iter().skip(start).take(end - start));
        }
    }
    output.push_str("...");
    output
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use super::*;

    // Not upstream's: keys are cut to their ends by length.
    #[test]
    fn hides_api_keys() {
        for (key, want) in [
            ("", ""),
            ("ab", "ab"),
            ("abc", "a...c"),
            ("abcde", "ab...de"),
            ("abcdefgh", "ab...gh"),
            ("abcdefghi", "abcd...fghi"),
            ("sk-0123456789", "sk-0...6789"),
        ] {
            assert_eq!(hide_api_key(key), want, "{key}");
        }
        assert_eq!(hide_bytes("ééééé".as_bytes()), {
            let mut want = "éé".as_bytes().to_vec();
            want.extend_from_slice(b"...");
            want.extend_from_slice("éé".as_bytes());
            want
        });
        let broken = hide_api_key("aaaé999999");
        assert!(broken.starts_with('a'));
        assert!(broken.contains(char::REPLACEMENT_CHARACTER));
    }

    // Not upstream's: a credential of one or two bytes, which upstream's
    // masks keep, is hidden whole in a header or a query.
    #[test]
    fn hides_tiny_credentials_whole() {
        assert_eq!(mask_header_value("X-Api-Key", "ab"), "...");
        assert_eq!(mask_header_value("Authorization", "Bearer x"), "Bearer ...");
        assert_eq!(mask_header_value("Cookie", "a"), "...");
        assert_eq!(mask_header_value("X-Api-Key", ""), "");
        assert_eq!(mask_header_value("X-Api-Key", "abc"), "a...c");
        assert_eq!(mask_sensitive_query("key=xy&alt=sse"), "key=...&alt=sse");
        assert_eq!(mask_sensitive_query("token=%41"), "token=...");
        assert_eq!(mask_sensitive_query("token="), "token=");
        assert_eq!(percent_unescape("a+b%40"), Some(b"a+b@".to_vec()));
        assert_eq!(query_unescape("a+b%40"), Some(b"a b@".to_vec()));
    }

    // Not upstream's: credential headers are masked, others kept.
    #[test]
    fn masks_sensitive_headers() {
        assert_eq!(
            mask_sensitive_header_value("Authorization", "Bearer sk-0123456789"),
            "Bearer sk-0...6789"
        );
        assert_eq!(
            mask_sensitive_header_value(" authorization ", "  sk-0123456789"),
            "  sk...6789"
        );
        assert_eq!(
            mask_sensitive_header_value("X-Goog-Api-Key", "AIza0123456789"),
            "AIza...6789"
        );
        assert_eq!(
            mask_sensitive_header_value("X-Session-Token", "abcdef"),
            "ab...ef"
        );
        assert_eq!(
            mask_sensitive_header_value("Cookie", "a=b; c=d"),
            "a=b; c=d"
        );
        assert_eq!(
            mask_sensitive_header_value("Content-Type", "application/json"),
            "application/json"
        );

        for name in [
            "Authorization",
            "x-api-key",
            "X-Goog-Api-Key",
            "Cookie",
            "Set-Cookie",
            "Proxy-Authorization",
            "X-Management-Key",
            "X-Local-Password",
            "x-refresh-token",
            "x-client-secret",
        ] {
            assert!(is_credential_header(name), "{name}");
        }
        for name in ["Content-Type", "User-Agent", "X-Request-Id", "Cookies"] {
            assert!(!is_credential_header(name), "{name}");
        }

        assert_eq!(
            mask_header_value("Cookie", "session=0123456789"),
            "sess...6789"
        );
        assert_eq!(
            mask_header_value("X-Management-Key", "management-key"),
            "mana...-key"
        );
        assert_eq!(
            mask_header_value("Authorization", "Bearer sk-0123456789"),
            "Bearer sk-0...6789"
        );
        assert_eq!(mask_header_value("Accept", "*/*"), "*/*");
    }

    // Not upstream's: key-like query parameters are masked, the rest of the
    // query kept as it was written.
    #[test]
    fn masks_sensitive_query_parameters() {
        assert_eq!(mask_sensitive_query(""), "");
        assert_eq!(mask_sensitive_query("alt=sse&x=%zz"), "alt=sse&x=%zz");
        assert_eq!(
            mask_sensitive_query("key=AIza0123456789&alt=sse"),
            "key=AIza...6789&alt=sse"
        );
        assert_eq!(
            mask_sensitive_query("alt=sse&&access_token=a%20b%2Bc+d0123"),
            "alt=sse&&access_token=a+b%2B...0123"
        );
        assert_eq!(
            mask_sensitive_query("Api%5FKey[]=abcdef"),
            "Api%5FKey[]=ab...ef"
        );
        assert_eq!(mask_sensitive_query("token"), "token=");
        assert_eq!(
            mask_sensitive_query("secret=%zz123456"),
            "secret=%25zz1...3456"
        );
        assert_eq!(
            mask_sensitive_query("token=%FF%FE%FD%FC"),
            "token=%FF...%FC"
        );
        assert_eq!(
            mask_sensitive_query("monkey=1234567890"),
            "monkey=1234567890"
        );
    }

    // Not upstream's: an OAuth callback's code and state, and the other
    // secrets beyond upstream's key-like names, are masked, their names
    // kept; names that only hold those words aren't.
    #[test]
    fn masks_oauth_and_other_secret_query_parameters() {
        assert_eq!(
            mask_sensitive_query("code=4/0AbCdEfGhIjKlMnOp&state=s7a8t9e0x1y2z3&scope=user"),
            "code=4%2F0A...MnOp&state=s7a8...y2z3&scope=user"
        );
        assert_eq!(mask_sensitive_query("State[]=ab"), "State[]=...");
        for (name, value, masked) in [
            ("auth", "abcdefghij", "abcd...ghij"),
            ("sig", "abcdefghij", "abcd...ghij"),
            ("X-Amz-Signature", "0123456789abcdef", "0123...cdef"),
            ("X-Amz-Credential", "AKIA1234%2F2026", "AKIA...2026"),
            ("code_verifier", "verifier-0123", "veri...0123"),
            ("PASSWORD", "hunter2xyz", "hunt...2xyz"),
            ("db_passwd", "hunter2xyz", "hunt...2xyz"),
            ("proxy-authorization", "Basic+dXNlcjpwYXNz", "Basi...YXNz"),
        ] {
            assert_eq!(
                mask_sensitive_query(&format!("{name}={value}&alt=sse")),
                format!("{name}={masked}&alt=sse"),
                "{name}"
            );
        }
        for kept in [
            "codes=123456789",
            "decode=123456789",
            "statement=123456789",
            "author=123456789",
            "signed=123456789",
            "authuser=0",
        ] {
            assert_eq!(mask_sensitive_query(kept), kept);
        }
    }

    // Not upstream's: every email in a text is masked to its first
    // characters and last label, wherever it stands.
    #[test]
    fn masks_every_email_in_a_text() {
        for (text, want) in [
            ("john.doe@example.com", "j***@e***.com"),
            (
                "from john@example.com and jane+tag@mail.example.co.uk, cc <x_y@corp-mail.io>",
                "from j***@e***.com and j***@m***.uk, cc <x***@c***.io>",
            ),
            (
                r"skipping auth file C:\Users\me\.cli-proxy-api\claude-john@example.com.json: denied",
                r"skipping auth file C:\Users\me\.cli-proxy-api\claude-j***@e***.com.json: denied",
            ),
            (
                "/home/me/.cli-proxy-api/codex-1a2b3c4d-john.doe@example.com-plus.json",
                "/home/me/.cli-proxy-api/codex-1a2b3c4d-j***@e***.com-plus.json",
            ),
            (
                "codex-john@example.com-team.json",
                "codex-j***@e***.com-team.json",
            ),
            (
                "-1a2b3c4d-john@example.com-k12.json",
                "-1***@e***.com-k12.json",
            ),
            (
                "claude-1a2b3c4d-john@example.com.json",
                "claude-1a2b3c4d-j***@e***.com.json",
            ),
            ("claude-deadbeef@example.com", "claude-d***@e***.com"),
            ("xai-john@example.com.cds", "xai-j***@e***.com.cds"),
            (
                ".claude-john@example.com.cds.4f2a91.tmp.cds",
                ".claude-j***@e***.com.cds.4f2a91.tmp.cds",
            ),
            (
                r#"auth_id="codex-john@example.com-plus.json" label=John@Example.COM"#,
                r#"auth_id="codex-j***@e***.com-plus.json" label=J***@E***.COM"#,
            ),
            (
                "GET \"/v0/management/auth-files/download?name=codex-john%40example.com-plus.json\"",
                "GET \"/v0/management/auth-files/download?name=codex-j***%40e***.com-plus.json\"",
            ),
            ("first%2Blast%40example.com", "f***%40e***.com"),
            ("josé.núñez@ejemplo.es", "j***@e***.es"),
            (
                "socks5://user:pass@proxy.example.com:1080",
                "socks5://user:p***@p***.com:1080",
            ),
        ] {
            assert_eq!(mask_emails(text), want, "{text}");
            assert_eq!(mask_emails(want), want, "masked again: {want}");
        }
        // An address's domain ends at the next `@`, which then has no local
        // part of its own.
        assert_eq!(mask_emails("a@b.com@c.com"), "a***@b***.com@c.com");
        // A local part starts after the at sign before it.
        assert_eq!(mask_emails("a@b@example.com"), "a@b***@e***.com");
        assert_eq!(mask_emails("x%40y%40example.com"), "x%40y***%40e***.com");
    }

    // Not upstream's: a long run of at signs is scanned once, not once for
    // each, so a client's path of `%40`s can't make the access line slow.
    #[test]
    fn long_runs_of_at_signs_are_scanned_once() {
        for unit in ["a%40", "%40", "a@", "@"] {
            let text = format!("/{}", unit.repeat(64 * 1024 / unit.len()));
            assert!(
                matches!(mask_emails(&text), Cow::Borrowed(same) if same == text),
                "{unit}"
            );
        }
        let text = format!("{}john@example.com", "a%40".repeat(16 * 1024));
        let want = format!("{}j***@e***.com", "a%40".repeat(16 * 1024));
        assert_eq!(mask_emails(&text), want);
    }

    // Not upstream's: a text without an email is returned as it is.
    #[test]
    fn leaves_texts_without_an_email_alone() {
        for text in [
            "",
            "no at sign at all",
            "100% done, 50%4 and %4",
            "user@localhost",
            "admin@127.0.0.1",
            "claude-3-5-sonnet@20240620",
            "claude-opus-4-1@20250805",
            "golang.org/x/net@v0.25.0",
            "npm i pkg@1.2.3-beta.4",
            "@scope/pkg and @mention",
            "trailing@",
            "a@b.c",
            "user@.com",
            "user@-x.com",
            "name@sha256:0123abcd",
            "j***@e***.com",
            "claude-j***@e***.com.json",
            "q=a%40b&r=%40",
        ] {
            assert!(
                matches!(mask_emails(text), Cow::Borrowed(same) if same == text),
                "{text}"
            );
        }
    }

    // Ports TestSafeDiagnosticForLogPreservesAccessTokenExpiredAndRedactsCredentials.
    #[test]
    fn safe_diagnostic_keeps_access_token_expired_and_redacts_credentials() {
        let diagnostic = concat!(
            "access token expired\n",
            "access_token=access-secret refresh token: refresh-secret Authorization=Bearer bearer-secret ",
            r#"Post "https://user:password@oauth.example/token?access_token=query-secret" via socks5://proxy-user:proxy-password@127.0.0.1:1080"#,
        );

        let got = safe_diagnostic_for_log(diagnostic);
        assert!(got.contains("access token expired"), "{got}");
        for secret in [
            "access-secret",
            "refresh-secret",
            "bearer-secret",
            "query-secret",
            "user:password",
            "proxy-user",
            "proxy-password",
        ] {
            assert!(!got.contains(secret), "leaked {secret}: {got}");
        }
        assert!(!got.contains(['\r', '\n']), "{got}");
        assert!(got.contains("[REDACTED]"), "{got}");
    }

    // Ports TestSafeDiagnosticForLogKeepsPlainAccessTokenExpiredMessage.
    #[test]
    fn safe_diagnostic_keeps_a_plain_access_token_expired_message() {
        assert_eq!(
            safe_diagnostic_for_log("access token expired"),
            "access token expired"
        );
    }

    // Ports TestSafeDiagnosticForLogBoundsLargeMessageAndRetainsTrailingSignal.
    #[test]
    fn safe_diagnostic_bounds_a_large_message_and_keeps_a_trailing_signal() {
        let diagnostic = format!(
            "{}access token expired\nforged log line",
            "upstream context ".repeat(1000)
        );
        let got = safe_diagnostic_for_log(&diagnostic);
        assert!(got.chars().count() <= DIAGNOSTIC_LIMIT + 3, "{got}");
        assert!(got.contains("access token expired"), "{got}");
        assert!(!got.contains(['\r', '\n']), "{got}");
        assert!(got.ends_with("..."), "{got}");
    }

    // Ports TestSafeDiagnosticForLogBoundsLargeGenericMessage.
    #[test]
    fn safe_diagnostic_bounds_a_large_generic_message() {
        let got = safe_diagnostic_for_log(&"x".repeat(900));
        assert_eq!(got.chars().count(), DIAGNOSTIC_LIMIT + 3);
        assert!(got.ends_with("..."));
    }

    /// An error with a message and maybe a source.
    #[derive(Debug)]
    struct Failure {
        message: String,
        source: Option<Box<Failure>>,
    }

    impl Failure {
        fn new(message: &str) -> Self {
            Self {
                message: message.to_owned(),
                source: None,
            }
        }
    }

    impl fmt::Display for Failure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.message)
        }
    }

    impl Error for Failure {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.source
                .as_deref()
                .map(|source| source as &(dyn Error + 'static))
        }
    }

    // Ports TestSafeErrorDiagnosticExtractsOnlyAllowlistedSignals.
    #[test]
    fn safe_error_diagnostic_extracts_only_allowlisted_signals() {
        for (error, want) in [
            (Failure::new("EOF"), &["EOF"][..]),
            (
                Failure::new("socks connect with unlabeled-secret: connection refused"),
                &["proxy=socks", "connection_refused"][..],
            ),
            (
                Failure::new(
                    r#"upstream status 400 error="invalid_request" request_id="req-123" unlabeled-secret"#,
                ),
                &["status=400"][..],
            ),
            (Failure::new("unlabeled-secret"), &[][..]),
        ] {
            let got = safe_error_diagnostic(&error);
            for part in want {
                assert!(got.contains(part), "{error}: {got}");
            }
            assert!(!got.contains("unlabeled-secret"), "{error}: {got}");
        }
        assert!(
            safe_error_diagnostic(&Failure::new("unlabeled-secret")).starts_with("error_type=")
        );
    }

    // Ports TestSafeErrorDiagnosticDoesNotExtractURLQueryValues, with an
    // error whose source is EOF in place of Go's url.Error.
    #[test]
    fn safe_error_diagnostic_does_not_extract_url_query_values() {
        let error = Failure {
            message: r#"Post "https://oauth.example/token?code=oauth-secret&error=error-secret&request_id=request-secret""#.to_owned(),
            source: Some(Box::new(Failure::new("EOF"))),
        };
        let got = safe_error_diagnostic(&error);
        assert!(got.contains("EOF"), "{got}");
        for secret in ["oauth-secret", "error-secret", "request-secret"] {
            assert!(!got.contains(secret), "leaked {secret}: {got}");
        }
        for field in ["oauth_error=", "request_id="] {
            assert!(!got.contains(field), "extracted {field}: {got}");
        }
    }

    // Not upstream's: I/O errors in the source chain give their signals.
    #[test]
    fn safe_error_diagnostic_reads_io_errors_in_the_chain() {
        let error = io::Error::new(io::ErrorKind::UnexpectedEof, "early end");
        assert_eq!(safe_error_diagnostic(&error), "unexpected_EOF");
        let error = io::Error::new(io::ErrorKind::TimedOut, "slow");
        assert_eq!(safe_error_diagnostic(&error), "timeout");
        let error = Failure {
            message: "request failed".to_owned(),
            source: Some(Box::new(Failure::new(
                "tcp connect: connection reset by peer",
            ))),
        };
        assert_eq!(safe_error_diagnostic(&error), "connection_reset");
    }
}
