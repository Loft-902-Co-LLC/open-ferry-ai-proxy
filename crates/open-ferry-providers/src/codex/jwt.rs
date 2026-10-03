// Ported from CLIProxyAPI internal/auth/codex/jwt_parser.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The claims in a Codex `id_token`: the account ID, email and plan.
//!
//! The signature isn't checked, as upstream doesn't check it: the token comes
//! from OpenAI's token endpoint over TLS, and is only read to name and file
//! the credential.
//!
//! Claims are decoded with Go's `encoding/json` rules, which upstream relies
//! on: keys match their field ignoring case, `null` leaves a field empty, and
//! a value of the wrong type fails the whole parse.
//!
//! Deviations from upstream:
//! - Error texts after the prefixes upstream writes (`failed to decode JWT
//!   claims: `, `failed to unmarshal JWT claims: `) are this module's own.

use base64::Engine;
use base64::alphabet::URL_SAFE;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use chrono::{DateTime, FixedOffset};
use serde_json::{Map, Value};

/// The plan of an account whose token names none.
pub const DEFAULT_PLAN_TYPE: &str = "free";

/// Go's `base64.URLEncoding`: padded, and lenient about trailing bits.
const GO_URL_ENCODING: GeneralPurpose = GeneralPurpose::new(
    &URL_SAFE,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical)
        .with_decode_allow_trailing_bits(true),
);

/// The claims of a Codex `id_token` (upstream's `JWTClaims`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JwtClaims {
    /// `at_hash`.
    pub at_hash: String,
    /// `aud`.
    pub aud: Vec<String>,
    /// `auth_provider`.
    pub auth_provider: String,
    /// `auth_time`.
    pub auth_time: i64,
    /// `email`.
    pub email: String,
    /// `email_verified`.
    pub email_verified: bool,
    /// `exp`.
    pub exp: i64,
    /// `https://api.openai.com/auth`: the ChatGPT account.
    pub codex_auth_info: CodexAuthInfo,
    /// `iat`.
    pub iat: i64,
    /// `iss`.
    pub iss: String,
    /// `jti`.
    pub jti: String,
    /// `rat`.
    pub rat: i64,
    /// `sid`.
    pub sid: String,
    /// `sub`.
    pub sub: String,
}

/// The ChatGPT account claims (upstream's `CodexAuthInfo`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodexAuthInfo {
    /// `chatgpt_account_id`.
    pub chatgpt_account_id: String,
    /// `chatgpt_plan_type`, such as `plus` or `team`.
    pub chatgpt_plan_type: String,
    /// `chatgpt_subscription_active_start`, as sent.
    pub chatgpt_subscription_active_start: Value,
    /// `chatgpt_subscription_active_until`, as sent.
    pub chatgpt_subscription_active_until: Value,
    /// `chatgpt_subscription_last_checked`.
    pub chatgpt_subscription_last_checked: Option<DateTime<FixedOffset>>,
    /// `chatgpt_user_id`.
    pub chatgpt_user_id: String,
    /// `groups`.
    pub groups: Vec<Value>,
    /// `organizations`.
    pub organizations: Vec<Organization>,
    /// `user_id`.
    pub user_id: String,
}

/// An organization the account belongs to (upstream's `Organizations`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Organization {
    /// `id`.
    pub id: String,
    /// `is_default`.
    pub is_default: bool,
    /// `role`.
    pub role: String,
    /// `title`.
    pub title: String,
}

impl JwtClaims {
    /// The account's email (`GetUserEmail`).
    pub fn user_email(&self) -> &str {
        &self.email
    }

    /// The ChatGPT account ID (`GetAccountID`).
    pub fn account_id(&self) -> &str {
        &self.codex_auth_info.chatgpt_account_id
    }

    /// The plan, trimmed, or [`DEFAULT_PLAN_TYPE`] when there is none
    /// (`GetPlanType`).
    pub fn plan_type(&self) -> String {
        plan_type_or_default(Some(self))
    }
}

/// The plan `claims` name, or [`DEFAULT_PLAN_TYPE`] (upstream's
/// `GetPlanType`, which also takes no claims).
pub fn plan_type_or_default(claims: Option<&JwtClaims>) -> String {
    match claims.map(|claims| claims.codex_auth_info.chatgpt_plan_type.trim()) {
        Some(plan) if !plan.is_empty() => plan.to_owned(),
        _ => DEFAULT_PLAN_TYPE.to_owned(),
    }
}

/// Reads the claims of a JWT without checking its signature
/// (`ParseJWTToken`).
pub fn parse_jwt_token(token: &str) -> Result<JwtClaims, String> {
    let parts: Vec<&str> = token.split('.').collect();
    let [_, claims, _] = parts.as_slice() else {
        return Err(format!(
            "invalid JWT token format: expected 3 parts, got {}",
            parts.len()
        ));
    };
    let data =
        base64_url_decode(claims).map_err(|e| format!("failed to decode JWT claims: {e}"))?;
    decode_claims(&data).map_err(|e| format!("failed to unmarshal JWT claims: {e}"))
}

/// Pads unpadded base64url and decodes it.
fn base64_url_decode(data: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let padding = match data.len() % 4 {
        2 => "==",
        3 => "=",
        _ => "",
    };
    GO_URL_ENCODING.decode(format!("{data}{padding}"))
}

fn decode_claims(data: &[u8]) -> Result<JwtClaims, String> {
    let value: Value = serde_json::from_slice(data).map_err(|e| e.to_string())?;
    let mut claims = JwtClaims::default();
    let Some(object) = object_or_null(&value, "JWTClaims")? else {
        return Ok(claims);
    };
    for (key, value) in object {
        let field = format!("JWTClaims.{key}");
        let field = field.as_str();
        match key_of(
            key,
            &[
                "at_hash",
                "aud",
                "auth_provider",
                "auth_time",
                "email",
                "email_verified",
                "exp",
                "https://api.openai.com/auth",
                "iat",
                "iss",
                "jti",
                "rat",
                "sid",
                "sub",
            ],
        ) {
            Some("at_hash") => set_string(&mut claims.at_hash, value, field)?,
            Some("aud") => {
                if let Some(items) = array_or_null(value, field)? {
                    claims.aud = items
                        .iter()
                        .map(|item| {
                            let mut text = String::new();
                            set_string(&mut text, item, field).map(|()| text)
                        })
                        .collect::<Result<_, _>>()?;
                }
            }
            Some("auth_provider") => set_string(&mut claims.auth_provider, value, field)?,
            Some("auth_time") => set_int(&mut claims.auth_time, value, field)?,
            Some("email") => set_string(&mut claims.email, value, field)?,
            Some("email_verified") => set_bool(&mut claims.email_verified, value, field)?,
            Some("exp") => set_int(&mut claims.exp, value, field)?,
            Some("https://api.openai.com/auth") => {
                decode_auth_info(&mut claims.codex_auth_info, value)?;
            }
            Some("iat") => set_int(&mut claims.iat, value, field)?,
            Some("iss") => set_string(&mut claims.iss, value, field)?,
            Some("jti") => set_string(&mut claims.jti, value, field)?,
            Some("rat") => set_int(&mut claims.rat, value, field)?,
            Some("sid") => set_string(&mut claims.sid, value, field)?,
            Some("sub") => set_string(&mut claims.sub, value, field)?,
            _ => {}
        }
    }
    Ok(claims)
}

fn decode_auth_info(info: &mut CodexAuthInfo, value: &Value) -> Result<(), String> {
    let Some(object) = object_or_null(value, "JWTClaims.https://api.openai.com/auth")? else {
        return Ok(());
    };
    for (key, value) in object {
        let field = format!("CodexAuthInfo.{key}");
        let field = field.as_str();
        match key_of(
            key,
            &[
                "chatgpt_account_id",
                "chatgpt_plan_type",
                "chatgpt_subscription_active_start",
                "chatgpt_subscription_active_until",
                "chatgpt_subscription_last_checked",
                "chatgpt_user_id",
                "groups",
                "organizations",
                "user_id",
            ],
        ) {
            Some("chatgpt_account_id") => set_string(&mut info.chatgpt_account_id, value, field)?,
            Some("chatgpt_plan_type") => set_string(&mut info.chatgpt_plan_type, value, field)?,
            Some("chatgpt_subscription_active_start") => {
                info.chatgpt_subscription_active_start = value.clone();
            }
            Some("chatgpt_subscription_active_until") => {
                info.chatgpt_subscription_active_until = value.clone();
            }
            Some("chatgpt_subscription_last_checked") => match value {
                Value::Null => {}
                Value::String(text) => {
                    let time = DateTime::parse_from_rfc3339(text)
                        .map_err(|e| format!("parsing time {text:?}: {e}"))?;
                    info.chatgpt_subscription_last_checked = Some(time);
                }
                _ => return Err("Time.UnmarshalJSON: input is not a JSON string".to_owned()),
            },
            Some("chatgpt_user_id") => set_string(&mut info.chatgpt_user_id, value, field)?,
            Some("groups") => {
                if let Some(items) = array_or_null(value, field)? {
                    info.groups = items.clone();
                }
            }
            Some("organizations") => {
                if let Some(items) = array_or_null(value, field)? {
                    info.organizations = items
                        .iter()
                        .map(decode_organization)
                        .collect::<Result<_, _>>()?;
                }
            }
            Some("user_id") => set_string(&mut info.user_id, value, field)?,
            _ => {}
        }
    }
    Ok(())
}

fn decode_organization(value: &Value) -> Result<Organization, String> {
    let mut organization = Organization::default();
    let Some(object) = object_or_null(value, "CodexAuthInfo.organizations")? else {
        return Ok(organization);
    };
    for (key, value) in object {
        let field = format!("Organizations.{key}");
        let field = field.as_str();
        match key_of(key, &["id", "is_default", "role", "title"]) {
            Some("id") => set_string(&mut organization.id, value, field)?,
            Some("is_default") => set_bool(&mut organization.is_default, value, field)?,
            Some("role") => set_string(&mut organization.role, value, field)?,
            Some("title") => set_string(&mut organization.title, value, field)?,
            _ => {}
        }
    }
    Ok(organization)
}

pub(super) use crate::json::key_of;

fn type_error(value: &Value, field: &str, go_type: &str) -> String {
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    format!("json: cannot unmarshal {kind} into Go struct field {field} of type {go_type}")
}

pub(super) fn object_or_null<'v>(
    value: &'v Value,
    field: &str,
) -> Result<Option<&'v Map<String, Value>>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Object(object) => Ok(Some(object)),
        _ => Err(type_error(value, field, "struct")),
    }
}

fn array_or_null<'v>(value: &'v Value, field: &str) -> Result<Option<&'v Vec<Value>>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Array(items) => Ok(Some(items)),
        _ => Err(type_error(value, field, "slice")),
    }
}

pub(super) fn set_string(target: &mut String, value: &Value, field: &str) -> Result<(), String> {
    match value {
        Value::Null => Ok(()),
        Value::String(text) => {
            text.clone_into(target);
            Ok(())
        }
        _ => Err(type_error(value, field, "string")),
    }
}

fn set_bool(target: &mut bool, value: &Value, field: &str) -> Result<(), String> {
    match value {
        Value::Null => Ok(()),
        Value::Bool(flag) => {
            *target = *flag;
            Ok(())
        }
        _ => Err(type_error(value, field, "bool")),
    }
}

/// Sets a Go `int`: only an integer literal in range fits.
pub(super) fn set_int(target: &mut i64, value: &Value, field: &str) -> Result<(), String> {
    match value {
        Value::Null => Ok(()),
        Value::Number(number) => {
            *target = number
                .to_string()
                .parse()
                .map_err(|_| type_error(value, field, "int"))?;
            Ok(())
        }
        _ => Err(type_error(value, field, "int")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;

    /// An unsigned JWT with `payload` as its claims (upstream's
    /// `makeTestJWT`).
    pub(crate) fn make_test_jwt(payload: &Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(payload.to_string());
        format!("{header}.{claims}.")
    }

    #[test]
    fn get_plan_type() {
        assert_eq!(plan_type_or_default(None), DEFAULT_PLAN_TYPE);
        assert_eq!(JwtClaims::default().plan_type(), DEFAULT_PLAN_TYPE);
        let mut claims = JwtClaims::default();
        claims.codex_auth_info.chatgpt_plan_type = "   ".into();
        assert_eq!(claims.plan_type(), DEFAULT_PLAN_TYPE);
        claims.codex_auth_info.chatgpt_plan_type = "pro".into();
        assert_eq!(claims.plan_type(), "pro");
    }

    #[test]
    fn missing_plan_type_defaults_to_free() {
        let without_plan = make_test_jwt(&json!({
            "email": "user@example.com",
            "https://api.openai.com/auth": {"chatgpt_account_id": "acc-12345"},
        }));
        let claims = parse_jwt_token(&without_plan).unwrap();
        assert_eq!(claims.plan_type(), "free");
        assert_eq!(claims.account_id(), "acc-12345");
        assert_eq!(claims.user_email(), "user@example.com");

        let with_plan = make_test_jwt(&json!({
            "email": "user@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acc-12345",
                "chatgpt_plan_type": "team",
            },
        }));
        assert_eq!(parse_jwt_token(&with_plan).unwrap().plan_type(), "team");
    }

    #[test]
    fn decodes_as_go_does() {
        let token = make_test_jwt(&json!({
            "EMAIL": "upper@example.com",
            "aud": ["app_x", null],
            "exp": 1700000000,
            "email_verified": null,
            "https://api.openai.com/auth": {
                "chatgpt_subscription_last_checked": "2026-01-02T03:04:05Z",
                "organizations": [{"id": "org-1", "is_default": true}],
                "groups": [1, "a"],
                "unknown": {"x": 1},
            },
        }));
        let claims = parse_jwt_token(&token).unwrap();
        assert_eq!(claims.email, "upper@example.com");
        assert_eq!(claims.aud, vec!["app_x".to_owned(), String::new()]);
        assert_eq!(claims.exp, 1_700_000_000);
        assert_eq!(claims.codex_auth_info.organizations[0].id, "org-1");
        assert!(
            claims
                .codex_auth_info
                .chatgpt_subscription_last_checked
                .is_some()
        );

        for bad in [
            json!({"exp": 1.5}),
            json!({"exp": "1"}),
            json!({"email": 1}),
            json!({"aud": "app_x"}),
            json!({"https://api.openai.com/auth": "x"}),
            json!({"https://api.openai.com/auth": {"chatgpt_subscription_last_checked": "yesterday"}}),
        ] {
            let error = parse_jwt_token(&make_test_jwt(&bad)).unwrap_err();
            assert!(
                error.starts_with("failed to unmarshal JWT claims: "),
                "{error}"
            );
        }
        // Go decodes a null into the zero value.
        assert_eq!(
            parse_jwt_token(&make_test_jwt(&Value::Null)).unwrap(),
            JwtClaims::default()
        );
    }

    #[test]
    fn rejects_malformed_tokens() {
        assert_eq!(
            parse_jwt_token("a.b").unwrap_err(),
            "invalid JWT token format: expected 3 parts, got 2"
        );
        assert!(
            parse_jwt_token("a.%%%.c")
                .unwrap_err()
                .starts_with("failed to decode JWT claims: ")
        );
        let not_json = format!("a.{}.c", URL_SAFE_NO_PAD.encode("nope"));
        assert!(
            parse_jwt_token(&not_json)
                .unwrap_err()
                .starts_with("failed to unmarshal JWT claims: ")
        );
        // Padded claims decode too, as Go's padded decoder takes them.
        let padded = format!(
            "a.{}.c",
            base64::engine::general_purpose::URL_SAFE.encode("{}")
        );
        assert!(parse_jwt_token(&padded).is_ok());
    }
}
