//! Keeps a valid JSON `null` distinct from an unavailable response request.

use serde_json::Value;
use std::sync::OnceLock;

/// The original request used when translating a provider's response.
///
/// Use `Option<&Value>` when parsing bytes: `Some(&Value::Null)` is valid JSON
/// and takes precedence over the translated request. Converting `&Value`
/// preserves the older API convention that `Null` means unavailable.
#[derive(Clone, Copy)]
pub struct ResponseRequest<'a> {
    parsed: Option<&'a Value>,
    raw: Option<&'a [u8]>,
    lazy: Option<&'a OnceLock<Option<Value>>>,
}

impl<'a> ResponseRequest<'a> {
    /// The parsed JSON, when the original request was available and valid.
    pub fn parsed(self) -> Option<&'a Value> {
        match self.lazy {
            Some(parsed) => parsed
                .get_or_init(|| super::exact::from_slice(self.raw.unwrap_or_default()).ok())
                .as_ref(),
            None => self.parsed,
        }
    }

    /// The JSON for readers that only inspect fields, or `Null` if unavailable.
    pub fn value(self) -> &'a Value {
        self.parsed().unwrap_or(&Value::Null)
    }

    /// Keeps the original bytes too, for translators whose upstream selects
    /// any nonempty original, including invalid JSON.
    pub fn from_bytes(raw: &'a [u8], parsed: Option<&'a Value>) -> Self {
        Self {
            parsed,
            raw: Some(raw),
            lazy: None,
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
            lazy: None,
        }
    }
}

impl<'a> From<&'a Value> for ResponseRequest<'a> {
    fn from(value: &'a Value) -> Self {
        Self::from((!value.is_null()).then_some(value))
    }
}

impl std::fmt::Debug for ResponseRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Preserve the previous Debug fields too; formatting is a consumer.
        f.debug_struct("ResponseRequest")
            .field("parsed", &self.parsed())
            .field("raw", &self.raw)
            .finish()
    }
}

/// Lazily owns the strict parsed value while retaining exact request bytes.
/// Production executors and parity use this same byte-oriented constructor.
pub struct ResponseRequestData<'a> {
    raw: &'a [u8],
    parsed: OnceLock<Option<Value>>,
}

impl<'a> ResponseRequestData<'a> {
    pub fn new(raw: &'a [u8]) -> Self {
        Self {
            raw,
            parsed: OnceLock::new(),
        }
    }

    pub fn as_request(&self) -> ResponseRequest<'_> {
        // Native responses and raw-byte translators may never inspect JSON.
        // Copies share the one strict parse, including cached parse failure;
        // valid null remains Some(Null), distinct from unavailable JSON.
        ResponseRequest {
            parsed: None,
            raw: Some(self.raw),
            lazy: Some(&self.parsed),
        }
    }

    pub fn value(&self) -> &Value {
        self.as_request().value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The eager constructor before parsing became lazy.
    fn old_request(raw: &[u8]) -> Option<Value> {
        super::super::exact::from_slice(raw).ok()
    }

    #[test]
    fn lazy_request_matches_the_eager_constructor() {
        let mut cases = vec![
            b"".to_vec(),
            b"null".to_vec(),
            b"false".to_vec(),
            b"not json".to_vec(),
            br#"{"model":"first","model":"last","n":18446744073709551617}"#.to_vec(),
            br#"{"s":"\u0061","input":[{"role":"user"}]}"#.to_vec(),
            vec![0xff],
        ];
        for n in 0..128 {
            cases.push(serde_json::to_vec(&serde_json::json!({"model":format!("m-{n}"),"stream":n%2==0,"input":[n,{"text":"hello"}]})).unwrap());
        }
        for raw in cases {
            let old = old_request(&raw);
            let data = ResponseRequestData::new(&raw);
            assert!(data.parsed.get().is_none());
            let request = data.as_request();
            assert_eq!(request.raw(), Some(raw.as_slice()));
            assert!(
                data.parsed.get().is_none(),
                "raw-only consumers never parse"
            );
            assert_eq!(request.parsed(), old.as_ref());
            assert_eq!(request.value(), old.as_ref().unwrap_or(&Value::Null));
            assert_eq!(data.value(), old.as_ref().unwrap_or(&Value::Null));
            assert_eq!(
                format!("{request:?}"),
                format!("{:?}", ResponseRequest::from_bytes(&raw, old.as_ref()))
            );
            let cached = data.parsed.get().unwrap();
            request.parsed();
            assert!(std::ptr::eq(cached, data.parsed.get().unwrap()));
        }
        assert!(
            ResponseRequestData::new(b"null")
                .as_request()
                .parsed()
                .is_some()
        );
        assert!(ResponseRequest::from(&Value::Null).parsed().is_none());
    }
}
