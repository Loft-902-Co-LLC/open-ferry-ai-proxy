//! Keeps a credential's secret out of the errors made from an upstream's
//! answers.
//!
//! An upstream's error body, or the payload of an error in its stream,
//! reaches the client as the error's message. A provider that quotes the key
//! it was sent ("Invalid API key: sk-...") would hand the proxy's key to
//! whoever called it, so every copy of the key, as it is or escaped as a
//! JSON string, becomes [`REDACTED`] first.
//!
//! Deviations from upstream: the whole module. Upstream passes error bodies
//! on as they came.

use std::borrow::Cow;

/// What a secret becomes.
pub(crate) const REDACTED: &str = "[redacted]";

/// The shortest secret that is redacted. A shorter one could be an ordinary
/// word of the message, and hiding it would garble the message.
const MIN_SECRET: usize = 8;

/// `body` with every copy of `secret` replaced by [`REDACTED`].
pub(crate) fn bytes<'a>(body: &'a [u8], secret: &str) -> Cow<'a, [u8]> {
    let mut out = Cow::Borrowed(body);
    for form in forms(secret) {
        if let Some(replaced) = replace(&out, form.as_bytes()) {
            out = Cow::Owned(replaced);
        }
    }
    out
}

/// `text` with every copy of `secret` replaced by [`REDACTED`].
pub(crate) fn text(text: String, secret: &str) -> String {
    let mut out = text;
    for form in forms(secret) {
        if out.contains(form.as_str()) {
            out = out.replace(form.as_str(), REDACTED);
        }
    }
    out
}

/// The forms of `secret` to look for: itself, and its JSON-escaped form if
/// that differs. None when it is too short to redact.
fn forms(secret: &str) -> Vec<String> {
    if secret.len() < MIN_SECRET {
        return Vec::new();
    }
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

/// `body` with every copy of `needle` replaced, or `None` when it has none.
fn replace(body: &[u8], needle: &[u8]) -> Option<Vec<u8>> {
    let mut rest = body;
    let mut out: Option<Vec<u8>> = None;
    while let Some(at) = rest
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let buffer = out.get_or_insert_with(|| Vec::with_capacity(body.len()));
        buffer.extend_from_slice(&rest[..at]);
        buffer.extend_from_slice(REDACTED.as_bytes());
        rest = &rest[at + needle.len()..];
    }
    let mut out = out?;
    out.extend_from_slice(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
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
}
