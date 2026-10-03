// Ported from gin-gonic/gin v1.10.1 binding/json.go (decodeJSON) (MIT) and
// Go's encoding/json decode.go and fold.go (Decoder.Decode into a struct,
// foldName) and scanner.go (maxNestingDepth) (go1.27, BSD-3-Clause), as
// CLIProxyAPI internal/api/handlers/management/api_tools.go (APICall) and
// quota.go (ResetQuota) use them through ShouldBindJSON (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/gin-gonic/gin
// https://github.com/golang/go

//! Request bodies, read as gin's `ShouldBindJSON` reads them into a struct
//! with Go's JSON decoder.
//!
//! The first JSON value in the body is read; anything after it is ignored.
//! It must be an object, or `null` for an empty struct. Keys match a
//! field's name exactly, else regardless of case, the first field
//! declared winning; other keys are skipped. Every occurrence of a key is
//! decoded in turn: `null` leaves a string field as it was, clears an
//! optional one, and clears a map; an object adds to the map. A value of
//! the wrong type fails the decode, as does a value nested over 10000
//! deep. Each byte of the body that isn't part of valid UTF-8 reads as
//! U+FFFD.
//!
//! Deviations from upstream:
//! - Bodies over 16 MiB are refused with a 413; upstream reads any size.
//! - A string holding an unpaired UTF-16 surrogate escape fails the decode;
//!   Go reads the surrogate as U+FFFD.

use std::fmt;
use std::marker::PhantomData;

use axum::body::Body;
use bytes::Bytes;
use http::StatusCode;
use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::go::lossy;

/// The most a management request body may hold.
pub(crate) const MAX_BODY: usize = 16 << 20;

/// How deep Go's decoder lets values nest (`maxNestingDepth`).
const MAX_DEPTH: usize = 10_000;

/// Reads a request body of at most [`MAX_BODY`] bytes, or answers 413.
pub(crate) async fn read_body(body: Body) -> Result<Bytes, axum::response::Response> {
    axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| crate::json::error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"))
}

/// A struct a body decodes into.
pub(crate) trait GoStruct: Default {
    /// The fields' JSON names, in declaration order.
    const FIELDS: &'static [&'static str];

    /// Decodes the next value of `map` into field `index`.
    fn set<'de, A: MapAccess<'de>>(&mut self, index: usize, map: &mut A) -> Result<(), A::Error>;
}

/// Decodes `body` as Go's `json.Decoder.Decode` would into `T`; `None` where
/// Go's decode fails.
pub(crate) fn decode<T: GoStruct>(body: &[u8]) -> Option<T> {
    if nests_too_deep(body) {
        return None;
    }
    let text = lossy(body);
    let mut deserializer = serde_json::Deserializer::from_str(&text);
    deserializer
        .deserialize_option(StructVisitor::<T>(PhantomData))
        .ok()
}

/// Whether the first value in `body` nests deeper than Go's decoder allows.
/// serde_json skips an unknown key's value however deep it nests. Only
/// brackets outside strings are counted: a body that isn't JSON fails to
/// decode anyway.
fn nests_too_deep(body: &[u8]) -> bool {
    let mut depth = 0;
    let mut in_string = false;
    let mut escaped = false;
    for &b in body {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
                if depth == 0 {
                    return false;
                }
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return true;
                }
            }
            b' ' | b'\t' | b'\n' | b'\r' => {}
            _ if depth == 0 => return false,
            b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    false
}

/// The field a key names: an exact match, else the first field equal to it
/// under Go's name folding.
fn field_index(fields: &[&str], key: &str) -> Option<usize> {
    fields
        .iter()
        .position(|field| *field == key)
        .or_else(|| fields.iter().position(|field| fold_matches(field, key)))
}

/// Whether `key` folds to the same name as the ASCII `field`, as Go's
/// `foldName` folds them: ASCII letters by case, and the two other
/// characters that fold to ASCII letters, the long s and the Kelvin sign.
fn fold_matches(field: &str, key: &str) -> bool {
    let mut chars = key.chars();
    for expected in field.bytes() {
        let folded = match chars.next() {
            Some('\u{17f}') => b'S',
            Some('\u{212a}') => b'K',
            Some(c) if c.is_ascii() => (c as u8).to_ascii_uppercase(),
            _ => return false,
        };
        if folded != expected.to_ascii_uppercase() {
            return false;
        }
    }
    chars.next().is_none()
}

struct StructVisitor<T>(PhantomData<T>);

impl<'de, T: GoStruct> Visitor<'de> for StructVisitor<T> {
    type Value = T;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object")
    }

    fn visit_none<E: de::Error>(self) -> Result<T, E> {
        Ok(T::default())
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<T, D::Error> {
        deserializer.deserialize_map(self)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<T, A::Error> {
        let mut value = T::default();
        while let Some(key) = map.next_key::<String>()? {
            match field_index(T::FIELDS, &key) {
                Some(index) => value.set(index, &mut map)?,
                None => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(value)
    }
}

/// A string field's value: `None` for `null`.
pub(crate) struct Nullable(pub(crate) Option<String>);

impl<'de> Deserialize<'de> for Nullable {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NullableVisitor;

        impl<'de> Visitor<'de> for NullableVisitor {
            type Value = Nullable;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string or null")
            }

            fn visit_none<E: de::Error>(self) -> Result<Nullable, E> {
                Ok(Nullable(None))
            }

            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Nullable, D::Error> {
                String::deserialize(d).map(|s| Nullable(Some(s)))
            }
        }

        deserializer.deserialize_option(NullableVisitor)
    }
}

/// A `map[string]string` field's value: `None` for `null`, else its
/// entries in order, a `null` value read as empty.
pub(crate) struct StringMap(pub(crate) Option<Vec<(String, String)>>);

impl<'de> Deserialize<'de> for StringMap {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MapVisitor;

        impl<'de> Visitor<'de> for MapVisitor {
            type Value = StringMap;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object of strings or null")
            }

            fn visit_none<E: de::Error>(self) -> Result<StringMap, E> {
                Ok(StringMap(None))
            }

            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<StringMap, D::Error> {
                d.deserialize_map(self)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<StringMap, A::Error> {
                let mut entries = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    let Nullable(value) = map.next_value()?;
                    entries.push((key, value.unwrap_or_default()));
                }
                Ok(StringMap(Some(entries)))
            }
        }

        deserializer.deserialize_option(MapVisitor)
    }
}

/// Sets a string field from the next value, leaving it as it was for
/// `null`.
pub(crate) fn set_string<'de, A: MapAccess<'de>>(
    field: &mut String,
    map: &mut A,
) -> Result<(), A::Error> {
    if let Nullable(Some(value)) = map.next_value()? {
        *field = value;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Debug, Default, PartialEq)]
    struct Sample {
        snake: Option<String>,
        camel: Option<String>,
        pascal: Option<String>,
        name: String,
        header: Option<BTreeMap<String, String>>,
    }

    impl GoStruct for Sample {
        const FIELDS: &'static [&'static str] =
            &["auth_index", "authIndex", "AuthIndex", "name", "header"];

        fn set<'de, A: MapAccess<'de>>(
            &mut self,
            index: usize,
            map: &mut A,
        ) -> Result<(), A::Error> {
            match index {
                0 => self.snake = map.next_value::<Nullable>()?.0,
                1 => self.camel = map.next_value::<Nullable>()?.0,
                2 => self.pascal = map.next_value::<Nullable>()?.0,
                3 => set_string(&mut self.name, map)?,
                _ => match map.next_value::<StringMap>()?.0 {
                    None => self.header = None,
                    Some(entries) => self.header.get_or_insert_default().extend(entries),
                },
            }
            Ok(())
        }
    }

    fn sample(body: &str) -> Option<Sample> {
        decode(body.as_bytes())
    }

    fn header(entries: &[(&str, &str)]) -> Option<BTreeMap<String, String>> {
        Some(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn reads_the_first_value_only() {
        assert_eq!(sample(r#"{"name":"a"} trailing {"#).unwrap().name, "a");
        assert_eq!(sample("null"), Some(Sample::default()));
        assert_eq!(sample(" {} "), Some(Sample::default()));
        assert_eq!(sample(""), None);
        assert_eq!(sample("  "), None);
        assert_eq!(sample("[]"), None);
        assert_eq!(sample("1"), None);
        assert_eq!(sample(r#""x""#), None);
        assert_eq!(sample(r#"{"name":"a""#), None);
    }

    #[test]
    fn keys_match_exactly_then_by_folding() {
        let body = sample(r#"{"AuthIndex":"p","AUTHINDEX":"c","Auth_Index":"s"}"#).unwrap();
        assert_eq!(body.pascal.as_deref(), Some("p"));
        assert_eq!(body.camel.as_deref(), Some("c"));
        assert_eq!(body.snake.as_deref(), Some("s"));
        assert_eq!(sample(r#"{"NAME":"x"}"#).unwrap().name, "x");
        assert_eq!(sample("{\"n\u{e1}me\":\"x\"}").unwrap().name, "");
        assert!(fold_matches("Ks", "\u{212a}\u{17f}"));
        assert!(!fold_matches("ab", "a"));
    }

    #[test]
    fn every_occurrence_of_a_key_is_decoded() {
        let body = sample(r#"{"name":"a","name":null,"authIndex":"x","authIndex":null}"#);
        let body = body.unwrap();
        assert_eq!(body.name, "a");
        assert_eq!(body.camel, None);

        let body = sample(r#"{"header":{"A":"1","B":null},"header":{"C":"3","A":"4"}}"#);
        assert_eq!(
            body.unwrap().header,
            header(&[("A", "4"), ("B", ""), ("C", "3")])
        );
        let body = sample(r#"{"header":{"A":"1"},"header":null}"#);
        assert_eq!(body.unwrap().header, None);
    }

    #[test]
    fn wrong_types_fail() {
        assert_eq!(sample(r#"{"name":1}"#), None);
        assert_eq!(sample(r#"{"name":true}"#), None);
        assert_eq!(sample(r#"{"auth_index":5}"#), None);
        assert_eq!(sample(r#"{"header":"x"}"#), None);
        assert_eq!(sample(r#"{"header":{"A":1}}"#), None);
        assert_eq!(sample(r#"{"header":{"A":2,"A":"1"}}"#), None);
        assert_eq!(sample(r#"{"header":[]}"#), None);
        // Unknown keys may hold anything valid.
        let body = sample(r#"{"other":{"deep":[1,2e300,{"x":null}]},"name":"n"}"#);
        assert_eq!(body.unwrap().name, "n");
        assert_eq!(sample(r#"{"other":[1,}"#), None);
    }

    #[test]
    fn values_nest_at_most_ten_thousand_deep() {
        // Go's answers; the object is the first level.
        let nested = |levels: usize| "[".repeat(levels) + &"]".repeat(levels);
        let body = |levels| format!(r#"{{"x":{},"name":"a"}}"#, nested(levels));
        assert_eq!(sample(&body(9999)).unwrap().name, "a");
        assert_eq!(sample(&body(10000)), None);
        assert_eq!(sample(&format!(r#"{{"name":{}}}"#, nested(1))), None);
        // Brackets in strings don't count, nor do those after the first
        // value.
        let quoted = format!(r#"{{"x":"\"{}","name":"a"}}"#, "[".repeat(20000));
        assert_eq!(sample(&quoted).unwrap().name, "a");
        let after = format!(r#"{{"name":"a"}} {}"#, nested(20000));
        assert_eq!(sample(&after).unwrap().name, "a");
        assert_eq!(sample("]"), None);
        assert_eq!(sample("}{"), None);
    }

    #[test]
    fn broken_utf8_reads_as_replacement_characters() {
        let body = decode::<Sample>(b"{\"name\":\"a\xe2\x82b\xff\"}").unwrap();
        assert_eq!(body.name, "a\u{fffd}\u{fffd}b\u{fffd}");
    }
}
