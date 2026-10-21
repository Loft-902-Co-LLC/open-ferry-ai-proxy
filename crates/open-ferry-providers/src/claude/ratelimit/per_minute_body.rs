// Ported from CLIProxyAPI internal/runtime/executor/helps/claude_ratelimit.go
// (db1bab01, v8.0.36, MIT). https://github.com/router-for-me/CLIProxyAPI

//! Decode the three fields as Go's encoding/json does: case-insensitive names,
//! ordered duplicate assignments, merged nested objects and ignored nulls.

use std::fmt;

use open_ferry_translate::go::equal_fold;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

enum Key {
    Error,
    Type,
    Message,
    Other,
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl Visitor<'_> for KeyVisitor {
            type Value = Key;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON key")
            }
            fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Key, E> {
                // Go replaces invalid Unicode with U+FFFD. None of our three
                // field names can match a key containing that replacement.
                let Ok(key) = std::str::from_utf8(bytes) else {
                    return Ok(Key::Other);
                };
                Ok(if equal_fold(key, "error") {
                    Key::Error
                } else if equal_fold(key, "type") {
                    Key::Type
                } else if equal_fold(key, "message") {
                    Key::Message
                } else {
                    Key::Other
                })
            }
        }
        decoder.deserialize_bytes(KeyVisitor)
    }
}

// Only the recognized type and complete messages matter here. Decode bytes
// so malformed UTF-8 and lone surrogate escapes insert replacement characters
// instead of rejecting a value that a later duplicate may overwrite. No match
// contains U+FFFD, so its replacement count cannot affect recognition.
struct MatchString(String);

impl<'de> Deserialize<'de> for MatchString {
    fn deserialize<D: Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        struct StringVisitor;
        impl Visitor<'_> for StringVisitor {
            type Value = MatchString;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON string")
            }
            fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<MatchString, E> {
                Ok(MatchString(String::from_utf8_lossy(bytes).into_owned()))
            }
        }
        decoder.deserialize_bytes(StringVisitor)
    }
}

#[derive(Default)]
struct Body {
    error_type: String,
    message: String,
}

struct Decode<'a> {
    body: &'a mut Body,
    envelope: bool,
}

impl<'de> DeserializeSeed<'de> for Decode<'_> {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Decode<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an error object or null")
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<Key>()? {
            if self.envelope && matches!(key, Key::Error) {
                map.next_value_seed(Decode {
                    body: self.body,
                    envelope: false,
                })?;
            } else if !self.envelope && matches!(key, Key::Type) {
                if let Some(value) = map.next_value::<Option<MatchString>>()? {
                    self.body.error_type = value.0;
                }
            } else if !self.envelope && matches!(key, Key::Message) {
                if let Some(value) = map.next_value::<Option<MatchString>>()? {
                    self.body.message = value.0;
                }
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

pub(super) fn is_explicit(bytes: &[u8]) -> bool {
    let mut body = Body::default();
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    if (Decode {
        body: &mut body,
        envelope: true,
    })
    .deserialize(&mut decoder)
    .is_err()
        || decoder.end().is_err()
        || body.error_type != "rate_limit_error"
    {
        return false;
    }
    matches!(
        crate::json::lower_trim(&body.message).as_str(),
        "this request would exceed the requests per minute rate limit. please try again later."
            | "this request would exceed the input tokens per minute rate limit. please try again later."
            | "this request would exceed the output tokens per minute rate limit. please try again later."
    )
}
