// Ported from CLIProxyAPI internal/runtime/executor/helps/claude_ratelimit.go
// (db1bab01, v8.0.36, MIT). https://github.com/router-for-me/CLIProxyAPI

//! Decode the three fields as Go's encoding/json does: case-insensitive names,
//! ordered duplicate assignments, merged nested objects and ignored nulls.

use std::fmt;

use open_ferry_translate::go::equal_fold;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, Visitor};

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
        while let Some(key) = map.next_key::<String>()? {
            if self.envelope && equal_fold(&key, "error") {
                map.next_value_seed(Decode {
                    body: self.body,
                    envelope: false,
                })?;
            } else if !self.envelope && equal_fold(&key, "type") {
                if let Some(value) = map.next_value::<Option<String>>()? {
                    self.body.error_type = value;
                }
            } else if !self.envelope && equal_fold(&key, "message") {
                if let Some(value) = map.next_value::<Option<String>>()? {
                    self.body.message = value;
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
