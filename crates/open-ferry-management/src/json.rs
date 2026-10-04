// Ported from gin-gonic/gin v1.10.1 render/json.go (JSON.Render,
// WriteJSON) (MIT) and Go's encoding/json encode.go (Marshal of maps,
// structs, strings and float64s) and time's Time.MarshalJSON (go1.27,
// BSD-3-Clause), as CLIProxyAPI internal/api/handlers/management uses them
// through c.JSON (v8.0.10, MIT). Float64s are written with
// open_ferry_translate::go::json_float.
// https://github.com/router-for-me/CLIProxyAPI
// https://github.com/gin-gonic/gin
// https://github.com/golang/go

//! Response bodies, written as gin's `c.JSON` writes them with Go's
//! `encoding/json`: a map's keys sorted, a struct's fields in their order,
//! `<`, `>`, `&`, U+2028 and U+2029 escaped, each byte that isn't part of
//! valid UTF-8 written as U+FFFD, numbers decoded from JSON written as
//! float64s, and times in RFC 3339 with nanoseconds, trailing zeros
//! dropped.
//!
//! A float64 is written as Go's encoder writes it, with [`json_float`]: the
//! shortest decimal that reads back the same, the nearest when two are as
//! short, ties to even.
//!
//! A [`Json`]'s `Debug` shows its shape, kinds, keys and lengths, never a
//! string or a number, which may be a secret.
//!
//! Deviations from upstream:
//! - A value decoded into `any` comes from serde_json, which read it with
//!   the credential: an integer `-0` is written back as `0`, where Go
//!   writes `-0`, and a number beyond float64's range, which Go fails to
//!   decode, is written as it was read.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode, header};
use open_ferry_core::auth::Timestamp;
use open_ferry_translate::go::json_float;
use serde_json::Value;

use crate::go::decode_rune;

/// A value to write as JSON. Its `Debug` shows only its shape.
#[derive(Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Int(i64),
    /// A `uint64`.
    Uint(u64),
    /// A string.
    Str(String),
    /// A Go string, which may hold bytes that aren't UTF-8.
    Bytes(Vec<u8>),
    /// A `time.Time`.
    Time(Timestamp),
    Array(Vec<Json>),
    /// A struct: fields in declaration order.
    Struct(Vec<(&'static str, Json)>),
    /// A map, or a `gin.H`: keys in byte order.
    Map(BTreeMap<String, Json>),
    /// A value decoded into Go's `any`: objects sorted, numbers float64.
    Any(Value),
}

/// Kinds, keys and lengths: a response body may hold a credential's token
/// or a secret in any string or number.
impl fmt::Debug for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("Null"),
            Self::Bool(_) => f.write_str("Bool(..)"),
            Self::Int(_) => f.write_str("Int(..)"),
            Self::Uint(_) => f.write_str("Uint(..)"),
            Self::Str(s) => f.debug_struct("Str").field("len", &s.len()).finish(),
            Self::Bytes(bytes) => f.debug_struct("Bytes").field("len", &bytes.len()).finish(),
            Self::Time(_) => f.write_str("Time(..)"),
            Self::Array(items) => f.debug_tuple("Array").field(items).finish(),
            Self::Struct(fields) => f.debug_tuple("Struct").field(fields).finish(),
            Self::Map(entries) => f.debug_tuple("Map").field(entries).finish(),
            Self::Any(value) => f.debug_tuple("Any").field(&Shape(value)).finish(),
        }
    }
}

/// The `Debug` of a decoded value: kinds, object keys and string lengths.
struct Shape<'a>(&'a Value);

impl fmt::Debug for Shape<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Value::Null => f.write_str("Null"),
            Value::Bool(_) => f.write_str("Bool(..)"),
            Value::Number(_) => f.write_str("Number(..)"),
            Value::String(s) => f.debug_struct("String").field("len", &s.len()).finish(),
            Value::Array(items) => f.debug_list().entries(items.iter().map(Shape)).finish(),
            Value::Object(entries) => f
                .debug_map()
                .entries(entries.iter().map(|(key, value)| (key, Shape(value))))
                .finish(),
        }
    }
}

impl Json {
    /// A `gin.H` from its entries.
    pub(crate) fn map<const N: usize>(entries: [(&str, Json); N]) -> Self {
        Self::Map(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    }

    /// The value as JSON text.
    pub(crate) fn encode(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Self::Uint(n) => {
                let _ = write!(out, "{n}");
            }
            Self::Str(s) => write_string(out, s.as_bytes()),
            Self::Bytes(bytes) => write_string(out, bytes),
            Self::Time(time) => {
                out.push('"');
                out.push_str(&go_time(*time));
                out.push('"');
            }
            Self::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Self::Struct(fields) => {
                out.push('{');
                for (i, (name, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(out, name.as_bytes());
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
            Self::Map(entries) => {
                out.push('{');
                for (i, (key, value)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(out, key.as_bytes());
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
            Self::Any(value) => write_any(out, value),
        }
    }
}

/// Writes a value decoded into `any` as Go writes it back.
fn write_any(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(number) => match number.as_f64() {
            Some(f) => out.push_str(&json_float(f)),
            // Out of float64's range; Go wouldn't have decoded it.
            None => out.push_str(&number.to_string()),
        },
        Value::String(s) => write_string(out, s.as_bytes()),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_any(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key.as_bytes());
                out.push(':');
                write_any(out, item);
            }
            out.push('}');
        }
    }
}

/// A time as Go's `MarshalJSON` writes a UTC time: RFC 3339 with up to
/// nine digits of fraction, trailing zeros dropped.
pub(crate) fn go_time(time: Timestamp) -> String {
    let mut out = time.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = time.timestamp_subsec_nanos().min(999_999_999);
    if nanos != 0 {
        let fraction = format!("{nanos:09}");
        out.push('.');
        out.push_str(fraction.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

/// Writes `bytes` as a JSON string, escaped as Go escapes it for HTML.
pub(crate) fn write_string(out: &mut String, bytes: &[u8]) {
    out.push('"');
    let mut rest = bytes;
    while !rest.is_empty() {
        let Some((c, width)) = decode_rune(rest) else {
            push_unicode_escape(out, 0xfffd);
            rest = &rest[1..];
            continue;
        };
        rest = &rest[width..];
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' || matches!(c, '<' | '>' | '&' | '\u{2028}' | '\u{2029}') => {
                push_unicode_escape(out, u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Writes a backslash, `u` and four lowercase hex digits.
fn push_unicode_escape(out: &mut String, code: u32) {
    out.push('\\');
    let _ = write!(out, "u{code:04x}");
}

/// A response with `body` as JSON, as gin's `c.JSON` sends it.
pub(crate) fn response(status: StatusCode, body: &Json) -> Response {
    let mut response = (status, body.encode()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

/// `{"error":message}` with `status`.
pub(crate) fn error(status: StatusCode, message: &str) -> Response {
    response(
        status,
        &Json::map([("error", Json::Str(message.to_owned()))]),
    )
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono::Utc;
    use serde_json::json;

    use super::*;

    /// A backslash, `u` and `hex`, built so the source holds no escape.
    fn u(hex: &str) -> String {
        format!("{}u{hex}", '\\')
    }

    /// Not upstream's: a value's `Debug` shows its kinds, keys and lengths,
    /// never a string, a number or a time.
    #[test]
    fn debug_shows_only_the_shape() {
        let value = Json::Struct(vec![
            ("access_token", Json::Str("TOKEN-SECRET".to_owned())),
            ("body", Json::Bytes(b"BYTES-SECRET".to_vec())),
            ("int", Json::Int(4_242_424_242)),
            ("uint", Json::Uint(5_353_535_353)),
            ("disabled", Json::Bool(true)),
            (
                "at",
                Json::Time(Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()),
            ),
            (
                "list",
                Json::Array(vec![Json::Str("ITEM-SECRET".to_owned()), Json::Null]),
            ),
            (
                "header",
                Json::map([("authorization", Json::Str("MAP-SECRET".to_owned()))]),
            ),
            (
                "metadata",
                Json::Any(json!({
                    "api_key": "ANY-SECRET",
                    "expires": 6_464_646_464.5,
                    "nested": [{"refresh": "DEEP-SECRET"}, false],
                })),
            ),
        ]);
        let shown = [format!("{value:?}"), format!("{value:#?}")];
        for shown in &shown {
            for hidden in ["SECRET", "4242", "5353", "6464", "2026", "true", "false"] {
                assert!(!shown.contains(hidden), "{hidden} in {shown}");
            }
            for key in ["access_token", "authorization", "api_key", "refresh"] {
                assert!(shown.contains(key), "{key} not in {shown}");
            }
        }
        assert_eq!(
            format!(
                "{:?}",
                Json::Struct(vec![("k", Json::Str("abc".to_owned()))])
            ),
            r#"Struct([("k", Str { len: 3 })])"#
        );
        assert_eq!(
            format!("{:?}", Json::Any(json!({"k": ["ab", 1, null, true, {}]}))),
            r#"Any({"k": [String { len: 2 }, Number(..), Null, Bool(..), {}]})"#
        );
    }

    #[test]
    fn strings_are_escaped_as_go_escapes_them() {
        let mut out = String::new();
        write_string(&mut out, "a\"b\\c\n\r\t\u{8}\u{c}\u{1}\u{7f}".as_bytes());
        assert_eq!(
            out,
            format!(r#""a\"b\\c\n\r\t\b\f{}{}""#, u("0001"), '\u{7f}')
        );

        let mut out = String::new();
        write_string(&mut out, "<a>&\u{2028}\u{2029}\u{e9}".as_bytes());
        assert_eq!(
            out,
            format!(
                "\"{}a{}{}{}{}\u{e9}\"",
                u("003c"),
                u("003e"),
                u("0026"),
                u("2028"),
                u("2029")
            )
        );

        // One replacement per byte that isn't valid UTF-8.
        let mut out = String::new();
        write_string(&mut out, b"x\xe2\x82y\xff");
        assert_eq!(out, format!("\"x{0}{0}y{0}\"", u("fffd")));
    }

    #[test]
    fn floats_are_written_as_go_writes_them() {
        assert_eq!(json_float(1.0), "1");
        assert_eq!(json_float(-0.0), "-0");
        assert_eq!(json_float(0.5), "0.5");
        assert_eq!(json_float(1e20), "100000000000000000000");
        assert_eq!(json_float(1e21), "1e+21");
        assert_eq!(json_float(1.5e300), "1.5e+300");
        assert_eq!(json_float(0.000001), "0.000001");
        assert_eq!(json_float(1e-7), "1e-7");
        assert_eq!(json_float(-2.5e-10), "-2.5e-10");
        assert_eq!(json_float(1e-100), "1e-100");
        assert_eq!(json_float(123456789.0), "123456789");
    }

    #[test]
    fn times_drop_trailing_zeros() {
        let time = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        assert_eq!(go_time(time), "2026-01-02T03:04:05Z");
        let time = time + chrono::TimeDelta::nanoseconds(120_000_000);
        assert_eq!(go_time(time), "2026-01-02T03:04:05.12Z");
        let time = time + chrono::TimeDelta::nanoseconds(1);
        assert_eq!(go_time(time), "2026-01-02T03:04:05.120000001Z");
    }

    #[test]
    fn maps_sort_and_structs_keep_their_order() {
        let value = Json::map([
            ("b", Json::Int(1)),
            (
                "a",
                Json::Struct(vec![("z", Json::Null), ("y", Json::Bool(true))]),
            ),
        ]);
        assert_eq!(value.encode(), r#"{"a":{"z":null,"y":true},"b":1}"#);
    }

    #[test]
    fn any_values_are_written_back_as_go_decoded_them() {
        let value: Value =
            serde_json::from_str(r#"{"b":1.50,"a":[10000000000000000000000,true,null,"<"]}"#)
                .unwrap();
        assert_eq!(
            Json::Any(value).encode(),
            format!(r#"{{"a":[1e+22,true,null,"{}"],"b":1.5}}"#, u("003c"))
        );
        assert_eq!(Json::Any(json!(7)).encode(), "7");
    }

    /// Go's `json.Marshal` of a string holding these bytes.
    #[test]
    fn strings_match_go() {
        let cases: &[(&[u8], &str)] = &[
            (b"\x00", "\"\x5cu0000\""),
            (b"a\x00z", "\"a\x5cu0000z\""),
            (b"\x01", "\"\x5cu0001\""),
            (b"\x02", "\"\x5cu0002\""),
            (b"\x03", "\"\x5cu0003\""),
            (b"\x04", "\"\x5cu0004\""),
            (b"\x05", "\"\x5cu0005\""),
            (b"\x06", "\"\x5cu0006\""),
            (b"\x07", "\"\x5cu0007\""),
            (b"\x08", "\"\\b\""),
            (b"\t", "\"\\t\""),
            (b"\n", "\"\\n\""),
            (b"\x0b", "\"\x5cu000b\""),
            (b"\x0c", "\"\\f\""),
            (b"\r", "\"\\r\""),
            (b"\x0e", "\"\x5cu000e\""),
            (b"\x0f", "\"\x5cu000f\""),
            (b"\x10", "\"\x5cu0010\""),
            (b"\x11", "\"\x5cu0011\""),
            (b"\x12", "\"\x5cu0012\""),
            (b"\x13", "\"\x5cu0013\""),
            (b"\x14", "\"\x5cu0014\""),
            (b"\x15", "\"\x5cu0015\""),
            (b"\x16", "\"\x5cu0016\""),
            (b"\x17", "\"\x5cu0017\""),
            (b"\x18", "\"\x5cu0018\""),
            (b"\x19", "\"\x5cu0019\""),
            (b"\x1a", "\"\x5cu001a\""),
            (b"\x1b", "\"\x5cu001b\""),
            (b"\x1c", "\"\x5cu001c\""),
            (b"\x1d", "\"\x5cu001d\""),
            (b"\x1e", "\"\x5cu001e\""),
            (b"\x1f", "\"\x5cu001f\""),
            (b"a\x1fz", "\"a\x5cu001fz\""),
            (b" ", "\" \""),
            (b"\"", "\"\\\"\""),
            (b"&", "\"\x5cu0026\""),
            (b"<", "\"\x5cu003c\""),
            (b">", "\"\x5cu003e\""),
            (b"\\", "\"\\\\\""),
            (b"\x7f", "\"\x7f\""),
            (b"a\x7fz", "\"a\x7fz\""),
            (b"\x80", "\"\x5cufffd\""),
            (b"a\x80z", "\"a\x5cufffdz\""),
            (b"a\xc0z", "\"a\x5cufffdz\""),
            (b"\xc2", "\"\x5cufffd\""),
            (b"a\xe2z", "\"a\x5cufffdz\""),
            (b"a\xf0z", "\"a\x5cufffdz\""),
            (b"\xff", "\"\x5cufffd\""),
            (b"a\xffz", "\"a\x5cufffdz\""),
            (b"\xe2\x80\xa8", "\"\x5cu2028\""),
            (b"\xe2\x80\xa9", "\"\x5cu2029\""),
            (b"\xc3\xa9", "\"\u{e9}\""),
            (b"\xf4\x8f\xbf\xbf", "\"\u{10ffff}\""),
            (b"\xef\xbf\xbd", "\"\u{fffd}\""),
            (b"<a&b>", "\"\x5cu003ca\x5cu0026b\x5cu003e\""),
            (b"\x7f", "\"\x7f\""),
            (b"\xc2\x80", "\"\u{80}\""),
            (b"\xdf\xbf", "\"\u{7ff}\""),
            (b"\xe0\xa0\x80", "\"\u{800}\""),
            (b"\xef\xbf\xbf", "\"\u{ffff}\""),
            (b"\xf0\x90\x80\x80", "\"\u{10000}\""),
            (b"plain text", "\"plain text\""),
            (b"\"quoted\" \\ back", "\"\\\"quoted\\\" \\\\ back\""),
            (b"\xe2\x80\x8b", "\"\u{200b}\""),
            (b"\xef\xbb\xbf", "\"\u{feff}\""),
            (b"\xed\xa0\x80", "\"\x5cufffd\x5cufffd\x5cufffd\""),
            (b"\xed\xbf\xbf", "\"\x5cufffd\x5cufffd\x5cufffd\""),
            (b"\xc0\x80", "\"\x5cufffd\x5cufffd\""),
            (b"\xc1\xbf", "\"\x5cufffd\x5cufffd\""),
            (b"\xe0\x80\x80", "\"\x5cufffd\x5cufffd\x5cufffd\""),
            (b"\xe0\x9f\xbf", "\"\x5cufffd\x5cufffd\x5cufffd\""),
            (
                b"\xf0\x8f\xbf\xbf",
                "\"\x5cufffd\x5cufffd\x5cufffd\x5cufffd\"",
            ),
            (
                b"\xf4\x90\x80\x80",
                "\"\x5cufffd\x5cufffd\x5cufffd\x5cufffd\"",
            ),
            (
                b"\xf5\x80\x80\x80",
                "\"\x5cufffd\x5cufffd\x5cufffd\x5cufffd\"",
            ),
            (b"\xe2\x82", "\"\x5cufffd\x5cufffd\""),
            (b"\xe2\x82x", "\"\x5cufffd\x5cufffdx\""),
            (b"\xf0\x9f\x98", "\"\x5cufffd\x5cufffd\x5cufffd\""),
            (b"\xf0\x9f\x98\x80", "\"\u{1f600}\""),
            (b"\x80\x80", "\"\x5cufffd\x5cufffd\""),
            (b"\xc2", "\"\x5cufffd\""),
            (b"\xc2\xc2\xa9", "\"\x5cufffd\u{a9}\""),
            (b"\xff\xfe", "\"\x5cufffd\x5cufffd\""),
            (b"\xe2\x80\xa8\xe2\x80", "\"\x5cu2028\x5cufffd\x5cufffd\""),
        ];
        for &(bytes, want) in cases {
            let mut out = String::new();
            write_string(&mut out, bytes);
            assert_eq!(out, want, "{bytes:?}");
        }
    }

    /// Go's `json.Marshal` of float64s, of UTC times given in nanoseconds
    /// since 1970, and of documents decoded into `any`.
    #[test]
    fn numbers_times_and_documents_match_go() {
        let floats = [
            ("0", "0"),
            ("-0", "-0"),
            ("1", "1"),
            ("1.5", "1.5"),
            ("100", "100"),
            ("1e20", "100000000000000000000"),
            ("1e21", "1e+21"),
            ("123456789012345678901", "123456789012345680000"),
            ("1e-6", "0.000001"),
            ("1e-7", "1e-7"),
            ("0.000001", "0.000001"),
            ("0.0000001", "1e-7"),
            ("123e-9", "1.23e-7"),
            ("1.7976931348623157e308", "1.7976931348623157e+308"),
            ("5e-324", "5e-324"),
            ("2.5e-8", "2.5e-8"),
            ("-1e21", "-1e+21"),
            ("1e100", "1e+100"),
            ("0.1", "0.1"),
            ("0.30000000000000004", "0.30000000000000004"),
            ("9007199254740993", "9007199254740992"),
            ("12345678.9", "12345678.9"),
            ("1E5", "100000"),
            ("1e+30", "1e+30"),
            ("-1.5e-10", "-1.5e-10"),
            ("4.9e-324", "5e-324"),
            ("999999999999999999999", "1e+21"),
            ("1000000000000000000000", "1e+21"),
            ("0.00000099", "9.9e-7"),
            ("0.000000999999", "9.99999e-7"),
            ("-0.0", "-0"),
            ("1.0", "1"),
            ("100000000000000000000", "100000000000000000000"),
            ("1e-5", "0.00001"),
            ("12e-7", "0.0000012"),
            ("3.14159", "3.14159"),
            ("-2.5e+25", "-2.5e+25"),
            ("1.23456789e-7", "1.23456789e-7"),
            ("2e-308", "2e-308"),
            ("2.2250738585072014e-308", "2.2250738585072014e-308"),
            ("123456789", "123456789"),
            ("1e6", "1000000"),
            ("1e15", "1000000000000000"),
            ("1e16", "10000000000000000"),
            ("1e17", "100000000000000000"),
            ("2156163594508435.25", "2156163594508435.2"),
            ("-2.98023223876953125e-8", "-2.9802322387695312e-8"),
        ];
        for (text, want) in floats {
            assert_eq!(json_float(text.parse().unwrap()), want, "{text}");
        }
        let times = [
            (0, "1970-01-01T00:00:00Z"),
            (1, "1970-01-01T00:00:00.000000001Z"),
            (10, "1970-01-01T00:00:00.00000001Z"),
            (100, "1970-01-01T00:00:00.0000001Z"),
            (1000, "1970-01-01T00:00:00.000001Z"),
            (123456789, "1970-01-01T00:00:00.123456789Z"),
            (120000000, "1970-01-01T00:00:00.12Z"),
            (999999999, "1970-01-01T00:00:00.999999999Z"),
            (1700000000000000000, "2023-11-14T22:13:20Z"),
            (1700000000500000000, "2023-11-14T22:13:20.5Z"),
            (1700000000000000001, "2023-11-14T22:13:20.000000001Z"),
            (-1, "1969-12-31T23:59:59.999999999Z"),
            (1700000000010000000, "2023-11-14T22:13:20.01Z"),
            (1700000000000100000, "2023-11-14T22:13:20.0001Z"),
            (253402300799999999, "1978-01-11T21:31:40.799999999Z"),
            (-62135596800000000, "1968-01-12T20:06:43.2Z"),
        ];
        for (nanos, want) in times {
            let time = chrono::DateTime::from_timestamp_nanos(nanos);
            assert_eq!(go_time(time), want, "{nanos}");
        }
        let documents = [
            (
                "{\"b\":1,\"a\":[1.0,2.50,\"x\"],\"c\":null,\"d\":true}",
                "{\"a\":[1,2.5,\"x\"],\"b\":1,\"c\":null,\"d\":true}",
            ),
            (
                "{\"k\":\"<\x5cu2028>\"}",
                "{\"k\":\"\x5cu003c\x5cu2028\x5cu003e\"}",
            ),
            (
                "{\"\x5cu00e9\":1,\"e\":2,\"E\":3,\"_\":4,\"\":5}",
                "{\"\":5,\"E\":3,\"_\":4,\"e\":2,\"\u{e9}\":1}",
            ),
            ("[1,2]", "[1,2]"),
            (
                "{\"t\":[2156163594508435.25,-191224687729131.625,2.98023223876953125e-8]}",
                "{\"t\":[2156163594508435.2,-191224687729131.62,2.9802322387695312e-8]}",
            ),
            (
                "{\"a\":{\"b\":{\"c\":[{\"d\":\"\x5cu0000\"}]}}}",
                "{\"a\":{\"b\":{\"c\":[{\"d\":\"\x5cu0000\"}]}}}",
            ),
            ("{\"a\":1,\"a\":2}", "{\"a\":2}"),
            (
                "{\"big\":123456789012345678901234567890}",
                "{\"big\":1.2345678901234568e+29}",
            ),
            ("{\"s\":\"\\/\"}", "{\"s\":\"/\"}"),
            (
                "{\"t\":\"\x5cu00e9\x5cu0301\"}",
                "{\"t\":\"\u{e9}\u{301}\"}",
            ),
        ];
        for (text, want) in documents {
            let value: Value = serde_json::from_str(text).unwrap();
            assert_eq!(Json::Any(value).encode(), want, "{text}");
        }
    }
}
