// Ported from CLIProxyAPI internal/credentialweight/weight.go and
// sdk/cliproxy/auth/weight.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Credential weights: a credential's share of its provider's traffic under
//! weighted routing, from the `weight` attribute or metadata key.
//!
//! A missing or empty weight means [`DEFAULT_WEIGHT`]. Zero and negative
//! weights are valid and mean the credential takes no weighted traffic.
//!
//! Deviations from upstream:
//! - Metadata numbers are checked as the float64 Go decodes them to; Go's
//!   integer, `json.Number` and unsigned cases have no counterpart here.

use std::fmt;

use open_ferry_translate::go::quote;
use serde_json::{Map, Value};

use super::Auth;
use super::classification::ATTRIBUTE_WEIGHT;

/// The weight of a credential that doesn't set one.
pub const DEFAULT_WEIGHT: i64 = 1;

/// The largest weight allowed.
pub const MAX_WEIGHT: i64 = 1_000_000;

/// A weight that isn't a whole number in range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WeightError(String);

impl WeightError {
    fn not_integer() -> Self {
        Self("weight must be an integer".to_owned())
    }

    fn too_large() -> Self {
        Self(format!("weight must not exceed {MAX_WEIGHT}"))
    }

    fn context(self, context: &str) -> Self {
        Self(format!("{context}: {}", self.0))
    }
}

impl fmt::Display for WeightError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WeightError {}

/// Checks an explicit weight: zero or less becomes 0, above [`MAX_WEIGHT`]
/// is an error.
pub fn normalize_weight(weight: i64) -> Result<i64, WeightError> {
    if weight <= 0 {
        Ok(0)
    } else if weight > MAX_WEIGHT {
        Err(WeightError::too_large())
    } else {
        Ok(weight)
    }
}

/// Reads a weight attribute. Empty means [`DEFAULT_WEIGHT`].
pub fn parse_weight_str(raw: &str) -> Result<i64, WeightError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(DEFAULT_WEIGHT);
    }
    match raw.parse::<i64>() {
        Ok(weight) => normalize_weight(weight),
        Err(err) => {
            let reason = match err.kind() {
                std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => {
                    "value out of range"
                }
                _ => "invalid syntax",
            };
            Err(WeightError(format!(
                "weight must be an integer: strconv.ParseInt: parsing {}: {reason}",
                quote(raw)
            )))
        }
    }
}

/// Reads a weight from credential metadata: a whole number or a string
/// holding one.
pub fn parse_weight_value(value: &Value) -> Result<i64, WeightError> {
    match value {
        Value::Number(number) => {
            let Some(weight) = number.as_f64() else {
                return Err(WeightError::not_integer());
            };
            if weight.trunc() != weight {
                Err(WeightError::not_integer())
            } else if weight <= 0.0 {
                Ok(0)
            } else if weight > MAX_WEIGHT as f64 {
                Err(WeightError::too_large())
            } else {
                Ok(weight as i64)
            }
        }
        Value::String(raw) => parse_weight_str(raw),
        _ => Err(WeightError::not_integer()),
    }
}

/// Checks every explicit weight on `auth`, the attribute and then the
/// metadata key.
pub fn validate_auth_weight(auth: &Auth) -> Result<(), WeightError> {
    validate_weights(&auth.attributes, &auth.metadata)
}

/// [`validate_auth_weight`] on an auth's parts.
pub(crate) fn validate_weights(
    attributes: &std::collections::BTreeMap<String, String>,
    metadata: &Map<String, Value>,
) -> Result<(), WeightError> {
    if let Some(raw) = attributes.get(ATTRIBUTE_WEIGHT) {
        parse_weight_str(raw).map_err(|err| err.context("invalid attributes weight"))?;
    }
    if let Some(raw) = metadata.get(ATTRIBUTE_WEIGHT) {
        parse_weight_value(raw).map_err(|err| err.context("invalid metadata weight"))?;
    }
    Ok(())
}

/// Checks `auth`'s weights, then sets its weight attribute from the weight
/// in `metadata`, if there is one.
pub fn apply_auth_weight_metadata(
    auth: &mut Auth,
    metadata: &Map<String, Value>,
) -> Result<(), WeightError> {
    validate_auth_weight(auth)?;
    let Some(raw) = metadata.get(ATTRIBUTE_WEIGHT) else {
        return Ok(());
    };
    let weight = parse_weight_value(raw).map_err(|err| err.context("invalid metadata weight"))?;
    auth.attributes
        .insert(ATTRIBUTE_WEIGHT.to_owned(), weight.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn parse_value_validation() {
        assert_eq!(parse_weight_value(&Value::from("")), Ok(DEFAULT_WEIGHT));
        assert_eq!(parse_weight_value(&number("-5")), Ok(0));
        assert!(parse_weight_value(&number("1.5")).is_err());
        assert_eq!(parse_weight_value(&number("1000000")), Ok(MAX_WEIGHT));
        assert!(parse_weight_value(&number("1000001")).is_err());
        assert!(parse_weight_value(&number("9223372036854775808")).is_err());
        assert!(parse_weight_value(&Value::Bool(true)).is_err());
    }

    #[test]
    fn error_text_matches_go() {
        assert_eq!(
            parse_weight_str("heavy").unwrap_err().to_string(),
            r#"weight must be an integer: strconv.ParseInt: parsing "heavy": invalid syntax"#
        );
        assert_eq!(
            parse_weight_str("9223372036854775808")
                .unwrap_err()
                .to_string(),
            r#"weight must be an integer: strconv.ParseInt: parsing "9223372036854775808": value out of range"#
        );
        assert_eq!(
            parse_weight_value(&number("1000001"))
                .unwrap_err()
                .to_string(),
            "weight must not exceed 1000000"
        );
        assert_eq!(
            parse_weight_value(&number("1.5")).unwrap_err().to_string(),
            "weight must be an integer"
        );
    }

    #[test]
    fn validate_auth_weight_cases() {
        let attr = |value: &str| Auth {
            attributes: [(ATTRIBUTE_WEIGHT.to_owned(), value.to_owned())].into(),
            ..Auth::default()
        };
        let meta = |value: Value| Auth {
            metadata: [(ATTRIBUTE_WEIGHT.to_owned(), value)].into_iter().collect(),
            ..Auth::default()
        };
        assert!(validate_auth_weight(&Auth::default()).is_ok());
        assert!(validate_auth_weight(&attr("7")).is_ok());
        assert!(validate_auth_weight(&meta(number("0"))).is_ok());
        assert!(validate_auth_weight(&attr("-2")).is_ok());
        assert!(validate_auth_weight(&meta(number("1.5"))).is_err());
        assert!(validate_auth_weight(&attr("1000001")).is_err());
        assert!(validate_auth_weight(&meta(number("9223372036854775808"))).is_err());
        assert!(validate_auth_weight(&attr("invalid")).is_err());

        let mut both = attr("2");
        both.metadata
            .insert(ATTRIBUTE_WEIGHT.to_owned(), number("1.5"));
        assert_eq!(
            validate_auth_weight(&both).unwrap_err().to_string(),
            "invalid metadata weight: weight must be an integer"
        );
    }

    #[test]
    fn apply_sets_the_attribute_from_metadata() {
        let mut auth = Auth::default();
        let metadata: Map<String, Value> = serde_json::from_str(r#"{"weight":" 3 "}"#).unwrap();
        apply_auth_weight_metadata(&mut auth, &metadata).unwrap();
        assert_eq!(auth.attribute(ATTRIBUTE_WEIGHT), Some("3"));

        let metadata: Map<String, Value> = serde_json::from_str(r#"{"weight":-5}"#).unwrap();
        apply_auth_weight_metadata(&mut auth, &metadata).unwrap();
        assert_eq!(auth.attribute(ATTRIBUTE_WEIGHT), Some("0"));
    }
}
