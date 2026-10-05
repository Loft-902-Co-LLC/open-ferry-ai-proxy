// Ported from CLIProxyAPI internal/auth/codex/jwt_parser.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The claims in a Codex `id_token`: the account ID, email and plan.
//!
//! The signature isn't checked, as upstream doesn't check it: the token comes
//! from OpenAI's token endpoint over TLS, and is only read to name and file
//! the credential.
//!
//! Claims are decoded as Go's `encoding/json` decodes them, which upstream
//! relies on (see the crate's `go_json` module): keys match their field
//! ignoring case, `null` leaves a field as it was, a key that repeats
//! decodes into its field again, and a value of the wrong type fails the
//! whole parse. The payload's base64 may hold line breaks, which Go skips.
//!
//! Deviations from upstream:
//! - Error texts after the prefixes upstream writes (`failed to decode JWT
//!   claims: `, `failed to unmarshal JWT claims: `) are this module's own.
//! - A `chatgpt_subscription_active_start`, `chatgpt_subscription_active_until`
//!   or `groups` claim nested more than 128 deep fails the parse; Go reads it.

use base64::Engine;
use base64::alphabet::URL_SAFE;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use open_ferry_core::auth::{Timestamp, parse_go_rfc3339};
use serde_json::Value;

use crate::go_json::{self, Raw, Slice, any_value, object_or_null, set_bool, set_int, set_string};
use crate::json::key_of;

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
    /// `chatgpt_subscription_last_checked`, in UTC.
    pub chatgpt_subscription_last_checked: Option<Timestamp>,
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

/// Pads unpadded base64url and decodes it, skipping line breaks as Go's
/// decoder does. The padding goes by the length with the line breaks.
fn base64_url_decode(data: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let padding = match data.len() % 4 {
        2 => "==",
        3 => "=",
        _ => "",
    };
    let data: String = format!("{data}{padding}")
        .chars()
        .filter(|c| !matches!(c, '\r' | '\n'))
        .collect();
    GO_URL_ENCODING.decode(data)
}

/// The claims as Go's decoder builds them, with their slices' spare
/// elements.
#[derive(Default)]
struct Decoding {
    claims: JwtClaims,
    aud: Slice<String>,
    organizations: Slice<Organization>,
}

fn decode_claims(data: &[u8]) -> Result<JwtClaims, String> {
    let text = go_json::check(data)?;
    let mut decoding = Decoding::default();
    if let Some(members) = object_or_null(Raw::of(&text), "JWTClaims")? {
        for (key, value) in members {
            decode_claim(&mut decoding, &key, value)?;
        }
    }
    let Decoding {
        mut claims,
        aud,
        organizations,
    } = decoding;
    claims.aud = aud.into_vec();
    claims.codex_auth_info.organizations = organizations.into_vec();
    Ok(claims)
}

fn decode_claim(decoding: &mut Decoding, key: &str, value: Raw<'_>) -> Result<(), String> {
    let claims = &mut decoding.claims;
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
        Some("at_hash") => set_string(&mut claims.at_hash, value, field),
        Some("aud") => decoding.aud.decode(value, field, "[]string", |aud, item| {
            set_string(aud, item, field)
        }),
        Some("auth_provider") => set_string(&mut claims.auth_provider, value, field),
        Some("auth_time") => set_int(&mut claims.auth_time, value, field),
        Some("email") => set_string(&mut claims.email, value, field),
        Some("email_verified") => set_bool(&mut claims.email_verified, value, field),
        Some("exp") => set_int(&mut claims.exp, value, field),
        Some("https://api.openai.com/auth") => decode_auth_info(
            &mut claims.codex_auth_info,
            &mut decoding.organizations,
            value,
        ),
        Some("iat") => set_int(&mut claims.iat, value, field),
        Some("iss") => set_string(&mut claims.iss, value, field),
        Some("jti") => set_string(&mut claims.jti, value, field),
        Some("rat") => set_int(&mut claims.rat, value, field),
        Some("sid") => set_string(&mut claims.sid, value, field),
        Some("sub") => set_string(&mut claims.sub, value, field),
        _ => Ok(()),
    }
}

fn decode_auth_info(
    info: &mut CodexAuthInfo,
    organizations: &mut Slice<Organization>,
    value: Raw<'_>,
) -> Result<(), String> {
    let Some(members) = object_or_null(value, "JWTClaims.https://api.openai.com/auth")? else {
        return Ok(());
    };
    for (key, value) in members {
        let field = format!("CodexAuthInfo.{key}");
        let field = field.as_str();
        match key_of(
            &key,
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
                info.chatgpt_subscription_active_start = any_value(value)?;
            }
            Some("chatgpt_subscription_active_until") => {
                info.chatgpt_subscription_active_until = any_value(value)?;
            }
            Some("chatgpt_subscription_last_checked") => {
                set_time(&mut info.chatgpt_subscription_last_checked, value)?;
            }
            Some("chatgpt_user_id") => set_string(&mut info.chatgpt_user_id, value, field)?,
            Some("groups") => {
                let mut groups = Slice::default();
                groups.decode(value, field, "[]interface {}", |group, item| {
                    *group = any_value(item)?;
                    Ok(())
                })?;
                info.groups = groups.into_vec();
            }
            Some("organizations") => {
                organizations.decode(value, field, "[]codex.Organizations", decode_organization)?;
            }
            Some("user_id") => set_string(&mut info.user_id, value, field)?,
            _ => {}
        }
    }
    Ok(())
}

/// Decodes an organization into the one already there, as Go merges an
/// object into a struct.
fn decode_organization(organization: &mut Organization, value: Raw<'_>) -> Result<(), String> {
    let Some(members) = object_or_null(value, "CodexAuthInfo.organizations")? else {
        return Ok(());
    };
    for (key, value) in members {
        let field = format!("Organizations.{key}");
        let field = field.as_str();
        match key_of(&key, &["id", "is_default", "role", "title"]) {
            Some("id") => set_string(&mut organization.id, value, field)?,
            Some("is_default") => set_bool(&mut organization.is_default, value, field)?,
            Some("role") => set_string(&mut organization.role, value, field)?,
            Some("title") => set_string(&mut organization.title, value, field)?,
            _ => {}
        }
    }
    Ok(())
}

/// Sets a Go `time.Time` as its `UnmarshalJSON` does: `null` leaves it, and
/// a string must hold an RFC 3339 time as written, its escapes unread.
fn set_time(target: &mut Option<Timestamp>, value: Raw<'_>) -> Result<(), String> {
    if value.is_null() {
        return Ok(());
    }
    let text = value.text();
    let Some(time) = text
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
    else {
        return Err("Time.UnmarshalJSON: input is not a JSON string".to_owned());
    };
    let time = parse_go_rfc3339(time)
        .ok_or_else(|| format!("parsing time {text} as RFC 3339: cannot parse"))?;
    *target = Some(time);
    Ok(())
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

    /// A backslash, so that escapes are built rather than written.
    const BS: &str = "\\";
    const AUTH: &str = "\"https://api.openai.com/auth\"";

    /// An unsigned JWT with exactly `payload` as its claims.
    fn token_of(payload: &[u8]) -> String {
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(payload))
    }

    fn parse(payload: &str) -> Result<JwtClaims, String> {
        parse_jwt_token(&token_of(payload.as_bytes()))
    }

    /// What upstream's parser makes of these, as built with Go 1.26.
    #[test]
    fn decodes_odd_json_as_go_does() {
        let info = |payload: &str| parse(payload).unwrap().codex_auth_info;
        // A repeated struct merges; a `null` leaves it.
        let merged = info(&format!(
            r#"{{{AUTH}:{{"chatgpt_account_id":"account"}},{AUTH}:{{"chatgpt_plan_type":"team"}}}}"#
        ));
        assert_eq!(merged.chatgpt_account_id, "account");
        assert_eq!(merged.chatgpt_plan_type, "team");
        let kept = info(&format!(
            r#"{{{AUTH}:{{"chatgpt_account_id":"account"}},{AUTH}:null}}"#
        ));
        assert_eq!(kept.chatgpt_account_id, "account");

        // A repeated array reuses the elements a shorter one left.
        let aud = |payload: &str| parse(payload).unwrap().aud;
        assert_eq!(
            aud(r#"{"aud":["a","b"],"aud":["c"],"aud":["x",null]}"#),
            ["x", "b"]
        );
        assert_eq!(
            aud(r#"{"aud":["a","b"],"aud":[],"aud":[null,null]}"#),
            ["", ""]
        );
        assert_eq!(aud(r#"{"aud":["a"],"aud":null,"aud":[null]}"#), [""]);
        let organizations = info(&format!(
            r#"{{{AUTH}:{{"organizations":[{{"id":"a"}},{{"id":"b"}}]}},{AUTH}:{{"organizations":[{{"role":"r"}}]}},{AUTH}:{{"organizations":[{{}},null]}}}}"#
        ))
        .organizations;
        assert_eq!(
            organizations,
            [
                Organization {
                    id: "a".into(),
                    role: "r".into(),
                    ..Organization::default()
                },
                Organization {
                    id: "b".into(),
                    ..Organization::default()
                },
            ]
        );

        // An `any` is replaced each time.
        let groups = info(&format!(
            r#"{{{AUTH}:{{"groups":[{{"a":1}},2]}},{AUTH}:{{"groups":[{{"b":1}},null,3]}}}}"#
        ))
        .groups;
        assert_eq!(groups, [json!({"b": 1}), Value::Null, json!(3)]);
        let start = info(&format!(
            r#"{{{AUTH}:{{"chatgpt_subscription_active_start":{{"a":1,"a":2}},"chatgpt_subscription_active_start":{{"z":null}}}}}}"#
        ))
        .chatgpt_subscription_active_start;
        assert_eq!(start, json!({"z": null}));
        let start = info(&format!(
            r#"{{{AUTH}:{{"chatgpt_subscription_active_start":1,"chatgpt_subscription_active_start":null}}}}"#
        ))
        .chatgpt_subscription_active_start;
        assert_eq!(start, Value::Null);
        // Its numbers must fit a float64.
        let until = info(&format!(
            r#"{{{AUTH}:{{"chatgpt_subscription_active_until":[1e-400]}}}}"#
        ))
        .chatgpt_subscription_active_until;
        assert_eq!(until[0].as_f64(), Some(0.0));
        for bad in [
            format!(r#"{{{AUTH}:{{"chatgpt_subscription_active_until":1e400}}}}"#),
            format!(r#"{{{AUTH}:{{"groups":[{{"x":-1e309}}]}}}}"#),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }

        // Lone surrogates and invalid UTF-8 read as U+FFFD; keys are
        // unescaped before they're matched.
        let r = char::REPLACEMENT_CHARACTER;
        let grin = char::from_u32(0x1f600).unwrap();
        let email = parse(&format!(
            r#"{{"email":"{BS}ud800x{BS}ud83d{BS}ude00{BS}udc00{BS}ud800{BS}ud800{BS}n"}}"#
        ))
        .unwrap()
        .email;
        assert_eq!(email, format!("{r}x{grin}{r}{r}{r}\n"));
        let invalid = token_of(b"{\"email\":\"a\xe2\x82b\xed\xa0\x80c\xc0\x80\xffd\"}");
        assert_eq!(
            parse_jwt_token(&invalid).unwrap().email,
            format!("a{r}{r}b{r}{r}{r}c{r}{r}{r}d")
        );
        assert!(parse_jwt_token(&token_of(b"{\"email\":\"a\"\xff}")).is_err());
        let claims = parse(&format!(r#"{{"{BS}u0065MAIL":"k","EXP":-0}}"#)).unwrap();
        assert_eq!((claims.email.as_str(), claims.exp), ("k", 0));
        let start = info(&format!(
            r#"{{{AUTH}:{{"chatgpt_subscription_active_start":{{"k{BS}udfff":"{BS}ud800"}}}}}}"#
        ))
        .chatgpt_subscription_active_start;
        let want = serde_json::Map::from_iter([(format!("k{r}"), Value::String(r.into()))]);
        assert_eq!(start, Value::Object(want));

        // Values nest up to 10000 deep, even where nothing reads them.
        let deep = |depth: usize| {
            format!(
                r#"{{"x":{}{},"email":"e"}}"#,
                "[".repeat(depth),
                "]".repeat(depth)
            )
        };
        assert_eq!(parse(&deep(140)).unwrap().email, "e");
        assert!(parse(&deep(9_999)).is_ok());
        assert!(parse(&deep(10_000)).is_err());

        // A time is read as written, escapes and all, and the parse is
        // strict about case.
        let time = |text: &str| {
            parse(&format!(
                r#"{{{AUTH}:{{"chatgpt_subscription_last_checked":{text}}}}}"#
            ))
            .map(|claims| claims.codex_auth_info.chatgpt_subscription_last_checked)
        };
        assert!(time(r#""2026-10-03T12:00:00Z""#).unwrap().is_some());
        assert!(time(r#""2026-10-03T2:00:00,5+24:60""#).unwrap().is_some());
        assert_eq!(time("null").unwrap(), None);
        let escaped = format!(r#""2026-10-03T12:00:00{BS}u005a""#);
        for bad in [
            r#""2026-10-03t12:00:00z""#,
            escaped.as_str(),
            r#""2026-02-30T12:00:00Z""#,
            "5",
        ] {
            assert!(time(bad).is_err(), "{bad}");
        }

        let wrong_kinds = [
            format!(r#"{{{AUTH}:{{"organizations":["a"]}}}}"#),
            format!("{{{AUTH}:[]}}"),
            format!(r#"{{"email":"{BS}x"}}"#),
        ];
        for bad in wrong_kinds.iter().map(String::as_str).chain([
            r#"{"exp":1e3}"#,
            r#"{"exp":9223372036854775808}"#,
            r#""x""#,
            "{} x",
            r#"{"email":{"a":1}}"#,
            r#"{"aud":"a"}"#,
            r#"{"email":true}"#,
            r#"{"email_verified":"true"}"#,
            "{\"email\":\"a\x01\"}",
            r#"{"email":"a",}"#,
            r#"{"exp":01}"#,
        ]) {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert_eq!(parse(" {\"email\":\"w\"} \r\n\t").unwrap().email, "w");
    }

    #[test]
    fn skips_line_breaks_in_the_payload_as_go_does() {
        let payload = URL_SAFE_NO_PAD.encode(r#"{"email":"crlf@example.com"}"#);
        assert_eq!(payload.len() % 4, 2);
        let token = format!("e30.{}\r\n\r\n{}.sig", &payload[..4], &payload[4..]);
        assert_eq!(parse_jwt_token(&token).unwrap().email, "crlf@example.com");
        // The padding goes by the length with the line breaks, which here
        // leaves it wrong.
        let token = format!("e30.{}\n{}.sig", &payload[..4], &payload[4..]);
        assert!(parse_jwt_token(&token).is_err());
        // The unused bits of the last character are ignored.
        assert!(parse_jwt_token("e30.e31.sig").is_ok());
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
