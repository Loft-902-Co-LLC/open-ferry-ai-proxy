// Ported from CLIProxyAPI sdk/cliproxy/auth/selector.go (ExtractUpstreamErrorSummary
// and SanitizeUpstreamErrorSummary) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! A short, sanitized summary of a provider error, for the "last upstream
//! error" the manager's own errors carry.
//!
//! Credentials, URL user info, secret-looking key/value pairs and file paths
//! are redacted, and the result is at most 256 characters.
//!
//! Deviations from upstream:
//! - The patterns are upstream's with Go's ASCII-only `\s` and `\b` written
//!   out, since Rust's are Unicode-aware.
//! - Where lowercasing changes a string's length before a known error word,
//!   upstream may cut a character in half; here that match is skipped.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::text::{equal_fold, go_lower, str_of};

/// Go's `\s`.
const WS: &str = r"\t\n\f\r ";

fn re(pattern: &str) -> Regex {
    // The patterns are constants; a bad one fails every test.
    Regex::new(
        &pattern
            .replace("\\s", &format!("[{WS}]"))
            .replace("WSCLASS", WS),
    )
    .unwrap_or_else(|_| Regex::new("$^").unwrap_or_else(|_| unreachable!()))
}

static SCHEME_AUTH: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)((?:[A-Za-z0-9.+_\-]+:)?//)(?:[^:WSCLASS/@]+:[^@WSCLASS]+|[^@WSCLASS/]+)@")
});
static QUERY_PARAM: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)([?&][A-Za-z0-9_.-]*(?:key|token|secret|password|auth|sig|signature)=)[^&WSCLASS,\r\n;]+",
    )
});
static COOKIE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)(?-u:\b)(?:set-)?cookie\s*:[^\r\n]+"));
static AUTH_HEADER: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)(?-u:\b)authorization\s*[:=]\s*[^\r\n]+"));
static NATURAL_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i)(?-u:\b)([A-Za-z0-9_.-]*(?:api[ _-]?key|access[ _-]?token|client[ _-]?secret|private[ _-]?key|secret[ _-]?key|password|secret|token|credentials?|sessionid))",
        r#"\s*(?:(?:is|was|provided|used)?\s*[:= ]\s*|\s+is\s+|\s+was\s+|\s+provided\s+|\s+)"#,
        r#"(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|(?:[^\r\n;,|]+?(?:\s+(?:and|with|for|via)\s+|[,;|]|\r|\n|$)|[^\r\n;,|]+))"#,
    ))
});
static KV: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r#"(?i)((?:'|")?(?:[A-Za-z0-9_.-]*(?:key|token|secret|password|credential|credentials|bearer|sessionid|auth|signature|sig))(?:'|")?\s*[=:]\s*)"#,
        r#"(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|(?:[^\r\n;,|]+?(?:\s+(?:and|with|for|via)\s+|[,;|]|\r|\n|$)|[^\r\n;,|]+))"#,
    ))
});
static INVALID_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i)(?-u:\b)(invalid|bad|expired|unknown)\s+(?:api\s+key|access\s+token|refresh\s+token|token|key|secret|password|credentials?|bearer)",
        r#"\s*(?:[:= ]\s*)?(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[^WSCLASS,\r\n;]+)"#,
    ))
});
static SK_KEY: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?-u:\b)(?:sk-[A-Za-z0-9._~+/=-]{6,}|ghp_[A-Za-z0-9._~+/=-]{6,})(?-u:\b)")
});
static BEARER: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)(?-u:\b)(?:bearer|basic)\s+[A-Za-z0-9._~+/=-]+"));
static DOUBLE_QUOTED_PATH: LazyLock<Regex> = LazyLock::new(|| re(r#""/[^"\r\n]+""#));
static SINGLE_QUOTED_PATH: LazyLock<Regex> = LazyLock::new(|| re(r"'/[^'\r\n]+'"));
static BACKTICK_QUOTED_PATH: LazyLock<Regex> = LazyLock::new(|| re(r"`/[^`\r\n]+`"));
static PATH_CONNECTOR: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)\s+(to|from|into|onto|for|via|with|and)\s+/"));
static UNIX_PATH_STANDARD: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r#"(^|[WSCLASS\(\[\{<"';,=])"#,
        r#"(/(?:[^/WSCLASS"',;?#()<>{}\[\]]+(?:\s+[^/WSCLASS"',;?#()<>{}\[\]]+)*/)*"#,
        r#"[^/:WSCLASS"',;?#()<>{}\[\]]+(?::[^/:WSCLASS"',;?#()<>{}\[\]]+)?)"#,
    ))
});
static FILE_EXT_PATH: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r#"(^|[WSCLASS"'`(\[,;=])"#,
        r#"(/[^WSCLASS:"'`,;\])>]+(?:\s+[^WSCLASS:"'`,;\])>]+)*"#,
        r"\.(?:json|yaml|yml|key|pem|txt|log|toml|conf|env|crt|cer))",
    ))
});
static WINDOWS_PATH: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?i)(?-u:\b)[A-Za-z]:\\[^\r\n:,;'"<>]+"#));
static WINDOWS_UNC_PATH: LazyLock<Regex> =
    LazyLock::new(|| re(r#"\\\\[^\r\n:,;'"<>]+\\[^\r\n:,;'"<>]+"#));

/// Error words a path may come before, as in `open /a/b: permission denied`.
const KNOWN_ERROR_PREFIXES: [&str; 17] = [
    "permission denied",
    "no such file",
    "file not found",
    "access denied",
    "operation not permitted",
    "denied",
    "read-only",
    "is a directory",
    "not a directory",
    "cannot find",
    "no space",
    "connection refused",
    "timeout",
    "failed",
    "error",
    "not supported",
    "invalid argument",
];

/// A concise, sanitized summary of a provider's error text, preferring the
/// code and message of a JSON error body (upstream's
/// `ExtractUpstreamErrorSummary`).
pub(crate) fn extract_upstream_error_summary(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let mut json_part = raw;
    if let Some(index) = raw.find(": {")
        && index < 50
    {
        json_part = raw.get(index + 2..).unwrap_or_default().trim();
    }
    if open_ferry_translate::go::gjson_valid(json_part.as_bytes())
        && let Ok(parsed) = serde_json::from_str::<Value>(json_part)
    {
        let (mut code, mut message) = (String::new(), String::new());
        if let Some(error) = parsed.get("error") {
            match error {
                Value::Object(_) => {
                    code = str_of(error.get("code")).trim().to_owned();
                    if code.is_empty() {
                        code = str_of(error.get("type")).trim().to_owned();
                    }
                    message = str_of(error.get("message")).trim().to_owned();
                }
                Value::String(text) => message = text.trim().to_owned(),
                _ => {}
            }
        }
        if code.is_empty() && message.is_empty() {
            code = str_of(parsed.get("code")).trim().to_owned();
            if code.is_empty() {
                code = str_of(parsed.get("type")).trim().to_owned();
            }
            message = str_of(parsed.get("message")).trim().to_owned();
        }
        let summary = match (code.is_empty(), message.is_empty()) {
            (false, false) => {
                if equal_fold(&code, &message) || go_lower(&message).contains(&go_lower(&code)) {
                    message
                } else {
                    format!("{code}: {message}")
                }
            }
            (true, false) => message,
            (false, true) => code,
            (true, true) => String::new(),
        };
        if !summary.is_empty() {
            return sanitize_upstream_error_summary(&summary);
        }
    }
    sanitize_upstream_error_summary(raw)
}

/// The most characters a summary keeps before it is cut to 253 and "...".
const SUMMARY_LIMIT: usize = 256;

/// Redacts secrets and paths and bounds the length to 256 characters
/// (upstream's `SanitizeUpstreamErrorSummary`).
pub(crate) fn sanitize_upstream_error_summary(s: &str) -> String {
    let s = sanitize_no_truncate(s);
    if s.chars().count() > SUMMARY_LIMIT {
        let mut out: String = s.chars().take(253).collect();
        out.push_str("...");
        return out;
    }
    s
}

/// Upstream's `sanitizeUpstreamErrorSummaryNoTruncate`, except that it may
/// stop early once the text passes [`SUMMARY_LIMIT`] characters: the caller
/// then keeps only the first 253, which are already final.
///
/// Upstream recurses on the text after each connector ("copy /a to /b"),
/// so a long run of connectors recursed once per connector and rescanned
/// the rest each time. Here the connectors are a loop, and the early stop
/// bounds how many times the rest is rescanned.
fn sanitize_no_truncate(s: &str) -> String {
    let mut out = String::new();
    let mut out_chars = 0;
    let mut rest = s.to_owned();
    loop {
        let s = pre_redact(&rest);
        // Connector-separated paths, as in "copy /tmp/a TO /tmp/b: denied":
        // the text before the connector is sanitized on its own, and the
        // text after it, from the path's slash, goes round again.
        let Some(found) = PATH_CONNECTOR.find(&s) else {
            out.push_str(&post_redact(s));
            return out;
        };
        let first = sanitize_no_truncate(s.get(..found.start()).unwrap_or_default());
        let connector = s.get(found.start()..found.end() - 1).unwrap_or_default();
        out.push_str(&first);
        out.push_str(connector);
        out_chars += first.chars().count() + connector.chars().count();
        if out_chars > SUMMARY_LIMIT {
            return out;
        }
        rest = format!("/{}", s.get(found.end()..).unwrap_or_default());
    }
}

/// The redactions applied before looking for a path connector.
fn pre_redact(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    let mut s = SCHEME_AUTH
        .replace_all(s, "${1}[REDACTED_AUTH]@")
        .into_owned();
    s = QUERY_PARAM.replace_all(&s, "${1}[REDACTED]").into_owned();
    s = DOUBLE_QUOTED_PATH
        .replace_all(&s, "\"[REDACTED_PATH]\"")
        .into_owned();
    s = SINGLE_QUOTED_PATH
        .replace_all(&s, "'[REDACTED_PATH]'")
        .into_owned();
    s = BACKTICK_QUOTED_PATH
        .replace_all(&s, "`[REDACTED_PATH]`")
        .into_owned();
    s = WINDOWS_PATH.replace_all(&s, "[REDACTED_PATH]").into_owned();
    WINDOWS_UNC_PATH
        .replace_all(&s, "[REDACTED_PATH]")
        .into_owned()
}

/// The redactions applied to text with no path connector left.
fn post_redact(mut s: String) -> String {
    if s.is_empty() {
        return s;
    }
    // A path before the colon of an error: the first known error word, or
    // else the first ": ".
    let lower = go_lower(&s);
    let mut colon_index: Option<usize> = None;
    for word in KNOWN_ERROR_PREFIXES {
        if let Some(index) = lower.find(&format!(": {word}"))
            && s.is_char_boundary(index)
            && colon_index.is_none_or(|current| index < current)
        {
            colon_index = Some(index);
        }
    }
    if colon_index.is_none() {
        colon_index = s.find(": ");
    }
    if let Some(colon_index) = colon_index {
        s = redact_path_before_colon(&s, colon_index);
    }

    for _ in 0..3 {
        let next = UNIX_PATH_STANDARD
            .replace_all(&s, "${1}[REDACTED_PATH]")
            .into_owned();
        if next == s {
            break;
        }
        s = next;
    }
    s = FILE_EXT_PATH
        .replace_all(&s, "${1}[REDACTED_PATH]")
        .into_owned();
    s = COOKIE.replace_all(&s, "Cookie: [REDACTED]").into_owned();
    s = AUTH_HEADER
        .replace_all(&s, "Authorization: [REDACTED]")
        .into_owned();
    s = SK_KEY.replace_all(&s, "sk-[REDACTED]").into_owned();
    s = BEARER.replace_all(&s, "Bearer [REDACTED]").into_owned();
    s = INVALID_TOKEN
        .replace_all(&s, "${1} token [REDACTED]")
        .into_owned();
    s = NATURAL_SECRET
        .replace_all(&s, "${1}: [REDACTED]")
        .into_owned();
    KV.replace_all(&s, "${1}[REDACTED]").into_owned()
}

/// Replaces the path that starts the text before `colon_index`.
fn redact_path_before_colon(s: &str, colon_index: usize) -> String {
    let (Some(prefix), Some(suffix)) = (s.get(..colon_index), s.get(colon_index..)) else {
        return s.to_owned();
    };
    let bytes = prefix.as_bytes();
    let mut slash_index = None;
    for (i, &byte) in bytes.iter().enumerate() {
        if byte != b'/' {
            continue;
        }
        let before = i.checked_sub(1).and_then(|j| bytes.get(j)).copied();
        if before == Some(b'/') {
            continue;
        }
        let head = bytes.get(..i).unwrap_or_default();
        if i >= 6
            && (head.ends_with(b"http:/") || head.ends_with(b"https:/") || head.ends_with(b"://"))
        {
            continue;
        }
        if matches!(
            before,
            None | Some(b' ' | b'\t' | b'(' | b'[' | b'{' | b'<' | b'"' | b'\'' | b'`' | b'=')
        ) {
            slash_index = Some(i);
            break;
        }
    }
    let Some(slash_index) = slash_index else {
        return s.to_owned();
    };
    let lead = prefix.get(..slash_index).unwrap_or_default();
    let mut path = prefix.get(slash_index..).unwrap_or_default();
    let mut trail_len = 0;
    while let Some(stripped) = path.strip_suffix([')', ']', '}', '>']) {
        path = stripped;
        trail_len += 1;
    }
    let trail = prefix.get(prefix.len() - trail_len..).unwrap_or_default();
    let redacted = if path.contains(" /") {
        vec!["[REDACTED_PATH]"; path.split(" /").count()].join(" ")
    } else {
        "[REDACTED_PATH]".to_owned()
    };
    format!("{lead}{redacted}{trail}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upstream's TestExtractUpstreamErrorSummary_AuthPackageDirectSanitization:
    /// (input, a part the output must hold, the exact output or "", a part it
    /// must not hold or "").
    const CASES: &[(&str, &str, &str, &str)] = &[
        (
            "authorization: Bearer abcdef+TOPSECRET==",
            "Authorization: [REDACTED]",
            "",
            "TOPSECRET",
        ),
        (
            "authorization: Basic my-secret-basic-auth",
            "Authorization: [REDACTED]",
            "",
            "my-secret-basic-auth",
        ),
        (
            "authorization: ApiKey SUPERSECRET",
            "Authorization: [REDACTED]",
            "",
            "SUPERSECRET",
        ),
        (
            "Authorization: ApiKey first,SECONDSECRET",
            "Authorization: [REDACTED]",
            "",
            "SECONDSECRET",
        ),
        (
            r#"Authorization: Digest username="Mufasa", realm="myrealm", nonce="NONCE", uri="/dir/index.html", response="SIG""#,
            "Authorization: [REDACTED]",
            "",
            "NONCE",
        ),
        (
            r#"password="correct horse battery staple""#,
            "[REDACTED]",
            "",
            "correct horse battery staple",
        ),
        (
            r#"api_key="secret,value""#,
            "[REDACTED]",
            "",
            "secret,value",
        ),
        (
            "invalid key sk-live-secret-key-123456",
            "[REDACTED]",
            "",
            "live-secret-key",
        ),
        (
            "open /Users/alice/configs/auth.json: permission denied",
            "[REDACTED_PATH]",
            "",
            "alice",
        ),
        (
            r#"{"code":"oops","message":"invalid token SUPERSECRET"}"#,
            "[REDACTED]",
            "",
            "SUPERSECRET",
        ),
        ("'api_key'=SUPERSECRET", "[REDACTED]", "", "SUPERSECRET"),
        (
            "authorization=Bearer SUPERSECRET",
            "[REDACTED]",
            "",
            "SUPERSECRET",
        ),
        (
            "invalid API key SUPERSECRET",
            "[REDACTED]",
            "",
            "SUPERSECRET",
        ),
        (
            "invalid access token SUPERSECRET",
            "[REDACTED]",
            "",
            "SUPERSECRET",
        ),
        (
            "proxyconnect tcp: socks5://alice:PASSSECRET@proxy.internal:1080",
            "[REDACTED_AUTH]",
            "",
            "PASSSECRET",
        ),
        (
            "request failed: https://example.com?sig=SUPERSECRET",
            "[REDACTED]",
            "",
            "SUPERSECRET",
        ),
        (
            r#"{"code":"oops","message":"invalid token \"SUPERSECRET\""}"#,
            "[REDACTED]",
            "",
            "SUPERSECRET",
        ),
        (
            r#"{"code":"oops","message":"password=\"abc\\\"SUPERSECRET\""}"#,
            "[REDACTED]",
            "",
            "SUPERSECRET",
        ),
        (
            "upstream rejected API key: SUPERSECRET",
            "[REDACTED]",
            "",
            "SUPERSECRET",
        ),
        ("password is SUPERSECRET", "[REDACTED]", "", "SUPERSECRET"),
        (
            "open /mnt/secrets/alice: permission denied",
            "[REDACTED_PATH]",
            "",
            "alice",
        ),
        (
            r"open C:\Users\Alice Smith\secret.txt: permission denied",
            "[REDACTED_PATH]",
            "",
            "Alice Smith",
        ),
        (
            "Cookie: sessionid=COOKIESECRET",
            "Cookie: [REDACTED]",
            "",
            "COOKIESECRET",
        ),
        (
            "Set-Cookie: session=SETCOOKIESECRET",
            "Cookie: [REDACTED]",
            "",
            "SETCOOKIESECRET",
        ),
        (
            "private_key=PRIVATEKEYSECRET",
            "[REDACTED]",
            "",
            "PRIVATEKEYSECRET",
        ),
        (
            "https://user:PASSSECRET@example.com/api",
            "https://[REDACTED_AUTH]@",
            "",
            "PASSSECRET",
        ),
        (
            "/workspace/tenants/alice/oauth-cache",
            "[REDACTED_PATH]",
            "",
            "alice",
        ),
        (
            "open /opt/cli-proxy/auth/alice.json: failed",
            "[REDACTED_PATH]",
            "",
            "alice",
        ),
        (
            r"open C:\Users\alice\secret.txt: failed",
            "[REDACTED_PATH]",
            "",
            "alice",
        ),
        (
            "failed with api_key=secret-value-123 and token: my-secret-token",
            "[REDACTED]",
            "",
            "secret-value-123",
        ),
        (
            "Incorrect API key provided: SUPERSECRET",
            "[REDACTED]",
            "",
            "SUPERSECRET",
        ),
        ("credentials: SUPERSECRET", "[REDACTED]", "", "SUPERSECRET"),
        (
            "AWS_SECRET_ACCESS_KEY=SUPERSECRET",
            "[REDACTED]",
            "",
            "SUPERSECRET",
        ),
        (
            "//alice:PASSSECRET@example.com/api",
            "[REDACTED_AUTH]",
            "",
            "PASSSECRET",
        ),
        (
            "open /run/secrets/alice: permission denied",
            "[REDACTED_PATH]",
            "",
            "alice",
        ),
        (
            "open /custom/tenant/alice: permission denied",
            "[REDACTED_PATH]",
            "",
            "alice",
        ),
        (
            r"open \\server\share\alice\secret.txt: permission denied",
            "[REDACTED_PATH]",
            "",
            "alice",
        ),
        ("SERVICE_KEY=SUPERSECRET", "[REDACTED]", "", "SUPERSECRET"),
        ("OPENAI_KEY=SUPERSECRET", "[REDACTED]", "", "SUPERSECRET"),
        (
            "https://example.com?x-key=SUPERSECRET",
            "[REDACTED]",
            "",
            "SUPERSECRET",
        ),
        (
            "open /custom/tenant/Alice Smith/secret.txt: denied",
            "[REDACTED_PATH]",
            "open [REDACTED_PATH]: denied",
            "Smith",
        ),
        (
            "open /custom/租户/alice: denied",
            "[REDACTED_PATH]",
            "",
            "alice",
        ),
        (
            "open /alice: permission denied",
            "[REDACTED_PATH]",
            "",
            "alice",
        ),
        (
            "stat /客户: no such file or directory",
            "[REDACTED_PATH]",
            "",
            "客户",
        ),
        (
            r#"open "/tmp/customer:TOPSECRET/creds": denied"#,
            "[REDACTED_PATH]",
            "",
            "TOPSECRET",
        ),
        (
            "password = correct horse battery staple",
            "[REDACTED]",
            "",
            "battery staple",
        ),
        (
            "credentials: alice secret",
            "[REDACTED]",
            "",
            "alice secret",
        ),
        (
            r#"open "/Users/alice/password=foo/bar": denied"#,
            "[REDACTED_PATH]",
            "",
            "alice",
        ),
        (
            "open /tmp/customer:TOPSECRET/creds: denied",
            "[REDACTED_PATH]",
            "",
            "TOPSECRET",
        ),
        (
            "open /tmp/customer:TOPSECRET: denied",
            "[REDACTED_PATH]",
            "open [REDACTED_PATH]: denied",
            "TOPSECRET",
        ),
        (
            "rename /Users/alice/source /Users/bob/private-data: denied",
            "[REDACTED_PATH]",
            "rename [REDACTED_PATH] [REDACTED_PATH]: denied",
            "bob",
        ),
        (
            "rename /Users/Alice Smith/source /Users/bob/private-data: denied",
            "[REDACTED_PATH]",
            "rename [REDACTED_PATH] [REDACTED_PATH]: denied",
            "Alice Smith",
        ),
        (
            "open /tmp/Alice Smith.txt: denied",
            "[REDACTED_PATH]",
            "open [REDACTED_PATH]: denied",
            "Alice Smith",
        ),
        (
            "rename /tmp/Alice Smith /tmp/Bob Jones: denied",
            "[REDACTED_PATH]",
            "rename [REDACTED_PATH] [REDACTED_PATH]: denied",
            "Alice Smith",
        ),
        (
            "open /tmp/customer: TOPSECRET: denied",
            "[REDACTED_PATH]",
            "open [REDACTED_PATH]: denied",
            "TOPSECRET",
        ),
        (
            "open (/tmp/customer): denied",
            "[REDACTED_PATH]",
            "open ([REDACTED_PATH]): denied",
            "customer",
        ),
        (
            "open {/tmp/customer}: denied",
            "[REDACTED_PATH]",
            "open {[REDACTED_PATH]}: denied",
            "customer",
        ),
        (
            "open /tmp/config: permission denied: retry later",
            "[REDACTED_PATH]",
            "open [REDACTED_PATH]: permission denied: retry later",
            "config",
        ),
        (
            "open /tmp/config: permission denied: access denied",
            "[REDACTED_PATH]",
            "open [REDACTED_PATH]: permission denied: access denied",
            "config",
        ),
        (
            "copy /tmp/a   TO\t/tmp/b: denied",
            "[REDACTED_PATH]",
            "copy [REDACTED_PATH]   TO\t[REDACTED_PATH]: denied",
            "",
        ),
    ];

    #[test]
    fn sanitizes_as_upstream_does() {
        for &(input, mask, exact, forbidden) in CASES {
            let got = extract_upstream_error_summary(input);
            assert!(got.contains(mask), "{input:?} -> {got:?}, want {mask:?}");
            if !exact.is_empty() {
                assert_eq!(got, exact, "{input:?}");
            }
            if !forbidden.is_empty() {
                assert!(!got.contains(forbidden), "{input:?} leaked in {got:?}");
            }
            assert!(got.chars().count() <= 256, "{input:?}");
        }
    }

    /// Outputs of upstream's ExtractUpstreamErrorSummary, from Go.
    #[test]
    fn connectors_match_upstream() {
        let mut want = "[REDACTED_PATH] to ".repeat(13);
        want.push_str("[REDAC...");
        let cases = [
            (format!("{}/a: denied", "/a to ".repeat(3000)), want),
            (
                "copy /tmp/a TO /tmp/b: denied".to_owned(),
                "copy [REDACTED_PATH] TO [REDACTED_PATH]: denied".to_owned(),
            ),
            (
                format!("{} to /a to /b", "x".repeat(300)),
                format!("{}...", "x".repeat(253)),
            ),
            (
                format!("cp {} to /b with /c: failed", "/abcdefghij".repeat(30)),
                "cp [REDACTED_PATH] to [REDACTED_PATH] with [REDACTED_PATH]: failed".to_owned(),
            ),
            (
                "err for /a/b and /c/d via 'x' with QQ/qQQ to /z token=abc: error"
                    .replace("QQ", "\""),
                "err for [REDACTED_PATH] and [REDACTED_PATH] via 'x' with QQ[REDACTED_PATH]QQ to [REDACTED_PATH]: error"
                    .replace("QQ", "\""),
            ),
        ];
        for (input, want) in cases {
            assert_eq!(extract_upstream_error_summary(&input), want, "{input:?}");
        }
    }

    #[test]
    fn many_connectors_stay_fast() {
        let input = "/ to ".repeat(200_000);
        let started = std::time::Instant::now();
        let got = extract_upstream_error_summary(&input);
        assert!(got.ends_with("..."), "{got}");
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    #[test]
    fn long_text_is_bounded() {
        let input = format!("{} to /tmp/x: denied", "a".repeat(300));
        let got = extract_upstream_error_summary(&input);
        assert!(got.ends_with("..."), "{got}");
        assert_eq!(got.chars().count(), 256);
        assert!(!got.contains('x'), "{got}");
    }

    #[test]
    fn summarizes_json_error_bodies() {
        // Upstream's TestErrorWithCause_ErrorDirectFormat.
        let cause = r#"{"type":"error","code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later.","sequence_number":0}"#;
        assert_eq!(
            extract_upstream_error_summary(cause),
            "server_is_overloaded: Our servers are currently overloaded. Please try again later."
        );
        assert_eq!(
            extract_upstream_error_summary(
                r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#
            ),
            "rate_limit_error: slow down"
        );
        assert_eq!(
            extract_upstream_error_summary(r#"{"error":"quota exceeded"}"#),
            "quota exceeded"
        );
        assert_eq!(
            extract_upstream_error_summary(
                r#"status 429: {"error":{"code":"rate_limit","message":"rate_limit hit"}}"#
            ),
            "rate_limit hit"
        );
        assert_eq!(extract_upstream_error_summary("  "), "");
    }
}
