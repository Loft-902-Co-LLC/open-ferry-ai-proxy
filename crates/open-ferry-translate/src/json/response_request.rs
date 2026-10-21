//! Keeps a valid JSON `null` distinct from an unavailable response request.

use serde_json::Value;

/// The original request used when translating a provider's response.
///
/// Use `Option<&Value>` when parsing bytes: `Some(&Value::Null)` is valid JSON
/// and takes precedence over the translated request. Converting `&Value`
/// preserves the older API convention that `Null` means unavailable.
#[derive(Clone, Copy, Debug)]
pub struct ResponseRequest<'a>(Option<&'a Value>);

impl<'a> ResponseRequest<'a> {
    /// The parsed JSON, when the original request was available and valid.
    pub fn parsed(self) -> Option<&'a Value> {
        self.0
    }

    /// The JSON for readers that only inspect fields, or `Null` if unavailable.
    pub fn value(self) -> &'a Value {
        self.0.unwrap_or(&Value::Null)
    }
}

impl<'a> From<Option<&'a Value>> for ResponseRequest<'a> {
    fn from(value: Option<&'a Value>) -> Self {
        Self(value)
    }
}

impl<'a> From<&'a Value> for ResponseRequest<'a> {
    fn from(value: &'a Value) -> Self {
        Self((!value.is_null()).then_some(value))
    }
}
