// Ported from CLIProxyAPI internal/api/handlers/management/config_v8.go
// (configV8ETag, ifMatchSatisfied, splitEntityTagList) (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The version token of the config file the v8 config routes give and take.
//!
//! A read of `/v8/management/config`, `config/*path` or `config.yaml`
//! answers an `ETag`: the SHA-256 of the file's bytes, in lowercase hex and
//! in quotes, so a strong tag. A `PUT`, `PATCH` or `DELETE` there may send
//! `If-Match`; one that doesn't match the file as it is now answers 412
//! `{"error":"precondition_failed"}` with the current `ETag`, and changes
//! nothing. A write without `If-Match` stays unconditional.
//!
//! [`if_match_satisfied`] is RFC 9110's strong comparison, as upstream
//! writes it: repeated header lines are one list; an absent header always
//! passes; a header that is there but blank carries no tag and fails; the
//! whole field `*` matches any current version; weak and malformed tags
//! never match.

use axum::response::Response;
use http::{HeaderMap, HeaderValue, StatusCode, header};

use crate::json;

/// The `ETag` of the config file whose SHA-256 is `hash` (upstream's
/// `configV8ETag`): a strong tag.
pub(crate) fn etag(hash: &str) -> String {
    format!("\"{hash}\"")
}

/// `response` with the `ETag` of the file whose SHA-256 is `hash`.
pub(crate) fn with_etag(mut response: Response, hash: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(&etag(hash)) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// The 412 a write whose `If-Match` doesn't match answers, with the file's
/// current tag.
pub(crate) fn precondition_failed(current_hash: &str) -> Response {
    with_etag(
        json::error(StatusCode::PRECONDITION_FAILED, "precondition_failed"),
        current_hash,
    )
}

/// The lines of the request's `If-Match` header; empty when there is none.
/// A byte that isn't UTF-8 can't be part of a tag that matches.
pub(crate) fn if_match_lines(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::IF_MATCH)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect()
}

/// Whether the `If-Match` header `values` (its lines) pass against the
/// file whose SHA-256 is `current_hash` (upstream's `ifMatchSatisfied`).
pub(crate) fn if_match_satisfied(values: &[String], current_hash: &str) -> bool {
    if values.is_empty() {
        return true;
    }
    let field = values.join(",");
    let field = field.trim();
    if field == "*" {
        return true;
    }
    split_entity_tag_list(field).into_iter().any(|tag| {
        let tag = tag.trim();
        tag.len() >= 2
            && tag.starts_with('"')
            && tag.ends_with('"')
            && !tag[1..tag.len() - 1].contains('"')
            && &tag[1..tag.len() - 1] == current_hash
    })
}

/// Splits an `If-Match` field value on list commas. A comma inside a quoted
/// entity tag is tag content, not a separator (upstream's
/// `splitEntityTagList`).
fn split_entity_tag_list(field: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (i, byte) in field.bytes().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                parts.push(&field[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&field[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123abcd";

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    // Ported from TestConfigV8ETagIfMatch: the comparison cases it sends
    // as `If-Match`.
    #[test]
    fn if_match_compares_strongly() {
        let current = etag(HASH);
        // No header: unconditional.
        assert!(if_match_satisfied(&[], HASH));
        // The current tag, and the whole-field wildcard.
        assert!(if_match_satisfied(&lines(&[&current]), HASH));
        assert!(if_match_satisfied(&lines(&["*"]), HASH));
        // A stale tag, weak and malformed tags, a comma inside quotes, and a
        // wildcard that is only one member of a list.
        let weak = format!("W/{current}");
        let quoted_comma = format!("\"{HASH},\"");
        for value in [
            "\"stale\"",
            weak.as_str(),
            "not-an-etag",
            "\"unterminated",
            "\"a\"b\"",
            "\"stale,*,tag\"",
            "W/\"stale,*,tag\"",
            "\"stale\", *",
            quoted_comma.as_str(),
        ] {
            assert!(!if_match_satisfied(&lines(&[value]), HASH), "{value}");
        }
        // A header that is there but blank carries no tag.
        for blank in ["", "  \t  "] {
            assert!(!if_match_satisfied(&lines(&[blank]), HASH), "{blank:?}");
        }
        // Tags split across repeated header lines are one list.
        assert!(if_match_satisfied(&lines(&["\"stale\"", &current]), HASH));
        // A list with the current tag, whitespace and weak members passes.
        let listed = format!("\"stale\", {current} , {weak}");
        assert!(if_match_satisfied(&lines(&[&listed]), HASH));
    }

    // Not upstream's: the tag is the hash in quotes, and the 412 carries the
    // current one.
    #[test]
    fn the_tag_is_the_quoted_hash() {
        assert_eq!(etag(HASH), "\"0123abcd\"");
        let response = precondition_failed(HASH);
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(response.headers()[header::ETAG], "\"0123abcd\"");
    }
}
