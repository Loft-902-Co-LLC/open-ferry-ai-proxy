//! Keeps a valid JSON `null` distinct from an unavailable response request.

use serde_json::Value;

/// The original request used when translating a provider's response.
///
/// Use `Option<&Value>` when parsing bytes: `Some(&Value::Null)` is valid JSON
/// and takes precedence over the translated request. Converting `&Value`
/// preserves the older API convention that `Null` means unavailable.
#[derive(Clone, Copy, Debug)]
pub struct ResponseRequest<'a> {
    parsed: Option<&'a Value>,
    raw: Option<&'a [u8]>,
}

impl<'a> ResponseRequest<'a> {
    /// The parsed JSON, when the original request was available and valid.
    pub fn parsed(self) -> Option<&'a Value> {
        self.parsed
    }

    /// The JSON for readers that only inspect fields, or `Null` if unavailable.
    pub fn value(self) -> &'a Value {
        self.parsed.unwrap_or(&Value::Null)
    }

    /// Keeps the original bytes too, for translators whose upstream selects
    /// any nonempty original, including invalid JSON.
    pub fn from_bytes(raw: &'a [u8], parsed: Option<&'a Value>) -> Self {
        Self {
            parsed,
            raw: Some(raw),
        }
    }

    /// The original bytes, when supplied by a byte-oriented caller.
    pub fn raw(self) -> Option<&'a [u8]> {
        self.raw
    }
}

impl<'a> From<Option<&'a Value>> for ResponseRequest<'a> {
    fn from(value: Option<&'a Value>) -> Self {
        Self {
            parsed: value,
            raw: None,
        }
    }
}

impl<'a> From<&'a Value> for ResponseRequest<'a> {
    fn from(value: &'a Value) -> Self {
        Self::from((!value.is_null()).then_some(value))
    }
}

/// Owns the strict parsed value while retaining the exact request bytes.
/// Production executors and parity use this same byte-oriented constructor.
pub struct ResponseRequestData<'a> {
    raw: &'a [u8],
    parsed: Option<Value>,
}

impl<'a> ResponseRequestData<'a> {
    pub fn new(raw: &'a [u8]) -> Self {
        Self {
            raw,
            parsed: super::exact::from_slice(raw).ok(),
        }
    }

    pub fn as_request(&self) -> ResponseRequest<'_> {
        ResponseRequest::from_bytes(self.raw, self.parsed.as_ref())
    }

    pub fn value(&self) -> &Value {
        self.parsed.as_ref().unwrap_or(&Value::Null)
    }
}
