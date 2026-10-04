// Ported from CLIProxyAPI internal/watcher/synthesizer/file.go,
// SetOAuthModelAliasesAttribute and sanitizeOAuthModelAliases in
// sdk/cliproxy/auth/oauth_model_alias.go, SanitizeOAuthModelAlias in
// internal/config/config_normalization.go, and the plan-type read of
// ParseJWTToken and GetPlanType in internal/auth/codex/jwt_parser.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Records from the credential files at the top of the auth directory, as
//! the watcher loads them.
//!
//! This reads more of a file than listing a [`FileStore`] does: besides the
//! fields it sets the attributes routing uses.
//!
//! - `priority` and `weight` become attributes of those names (an invalid
//!   weight skips the file).
//! - `note` becomes the `note` attribute, trimmed.
//! - `headers` become `header:<name>` attributes.
//! - `model_aliases` (or legacy `model-aliases`) becomes the
//!   `model_aliases` attribute, sanitized.
//! - `excluded_models` (or legacy `excluded-models`), merged with the
//!   provider's `oauth-excluded-models`, becomes `excluded_models` and
//!   `excluded_models_hash`, and `auth_kind` is `oauth`.
//! - A codex file's `plan_type`, or else the plan in its ID token, becomes
//!   the `plan_type` attribute (`free` when the token has none).
//!
//! The only part the config plays is the provider's
//! `oauth-excluded-models`; [`apply_config_attributes`] sets it again on a
//! registered credential when the config is reloaded.
//!
//! Files of type `gemini` and files without a type are skipped. A Vertex AI
//! service-account file (type `vertex`, as upstream's
//! `VertexCredentialStorage` writes it) is read like any other: its
//! `service_account`, `project_id` and `location` stay in the metadata for
//! the Vertex executor, and its `email` is the label.
//!
//! Deviations from upstream:
//! - Plugin auth parsers, the fingerprint-profile attribute and Kimi's
//!   domain attributes aren't ported.
//! - The codex ID token is read for its plan claim only, where upstream
//!   decodes every claim into typed fields and falls back to `free` if any
//!   has the wrong type.
//! - Files over 8 MiB and file names that aren't valid UTF-8 are skipped.
//!
//! [`FileStore`]: crate::auth::FileStore

use std::fs;
use std::path::Path;

use serde_json::{Map, Value};

use super::super::classification::{
    ATTRIBUTE_MODEL_ALIASES, ATTRIBUTE_PATH, ATTRIBUTE_SOURCE, ATTRIBUTE_SOURCE_BACKEND,
    AUTH_KIND_OAUTH, AUTH_SOURCE_FILE,
};
use super::super::file_store::{clean_prefix, id_for, read_capped};
use super::super::go::{decode_jwt_segment_padded, equal_fold};
use super::super::json::{decode_field, fold_field, remarshaled_fold_values, unmarshal_object};
use super::super::metadata::{
    apply_auth_priority_metadata, apply_custom_headers_from_metadata, normalize_credential_metadata,
};
use super::super::path::join;
use super::super::weight::{apply_auth_weight_metadata, validate_weights};
use super::super::{Auth, Status};
use super::{SynthesisContext, SynthesisError, apply_auth_excluded_models_meta};

/// The attribute listing a credential's excluded models.
const ATTRIBUTE_EXCLUDED_MODELS: &str = "excluded_models";
/// The attribute holding the hash of a credential's excluded models.
const ATTRIBUTE_EXCLUDED_MODELS_HASH: &str = "excluded_models_hash";

/// The plan of a codex account whose token names none.
pub const DEFAULT_CODEX_PLAN_TYPE: &str = "free";

/// Records for the `*.json` files directly in the context's auth directory,
/// in name order. Files that can't be read or used are skipped; those with
/// an invalid setting are logged by name.
pub fn synthesize_file_auths(ctx: &SynthesisContext) -> Vec<Auth> {
    let mut out = Vec::new();
    if ctx.auth_dir.as_os_str().is_empty() {
        return out;
    }
    let Ok(entries) = fs::read_dir(&ctx.auth_dir) else {
        return out;
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| open_ferry_translate::go::to_lower(name).ends_with(".json"))
        .collect();
    names.sort();
    for name in names {
        let full = join(&ctx.auth_dir, Path::new(&name));
        let Ok(data) = read_capped(&full) else {
            continue;
        };
        if data.is_empty() {
            continue;
        }
        match synthesize_auth_file(ctx, &full, &data) {
            Ok(Some(auth)) => out.push(auth),
            Ok(None) => {}
            Err(err) => tracing::warn!(error = %err, "skipping auth file {name}"),
        }
    }
    out
}

/// The record for one credential file's contents, `data`, read from
/// `full_path`; `None` for contents to skip quietly: not a JSON object, or
/// with no type or type `gemini`.
pub fn synthesize_auth_file(
    ctx: &SynthesisContext,
    full_path: &Path,
    data: &[u8],
) -> Result<Option<Auth>, SynthesisError> {
    if data.is_empty() {
        return Ok(None);
    }
    let Ok(Some(mut metadata)) = unmarshal_object(data) else {
        return Ok(None);
    };
    let Some(path_str) = full_path.to_str() else {
        return Ok(None);
    };
    let base = base_name(full_path, path_str);
    normalize_credential_metadata(&mut metadata);
    validate_weights(&Default::default(), &metadata)
        .map_err(|err| SynthesisError::new(format!("invalid weight in {base}: {err}")))?;

    let mut provider = open_ferry_translate::go::to_lower(
        metadata
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim(),
    );
    if provider == "gemini" {
        provider = "gemini-cli".to_owned();
    }
    if provider.is_empty() || provider == "gemini-cli" {
        return Ok(None);
    }

    let label = metadata
        .get("email")
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
        .unwrap_or(&provider)
        .to_owned();
    let auth_dir = match ctx.auth_dir.to_str() {
        Some(dir) if dir.trim().is_empty() => Path::new(""),
        _ => ctx.auth_dir.as_path(),
    };
    let id = id_for(full_path, path_str, auth_dir);
    let proxy_url = metadata
        .get("proxy_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let prefix = metadata
        .get("prefix")
        .and_then(Value::as_str)
        .map(clean_prefix)
        .unwrap_or_default();
    let disabled = metadata
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let per_account_excluded = extract_excluded_models(&metadata);
    let per_account_aliases = extract_model_aliases(&metadata);
    let note = metadata
        .get("note")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|note| !note.is_empty())
        .map(str::to_owned);
    let plan_type = (provider == "codex")
        .then(|| codex_plan_type(&metadata))
        .flatten();

    let mut auth = Auth {
        id,
        file_name: base.to_owned(),
        provider,
        label,
        prefix,
        status: if disabled {
            Status::Disabled
        } else {
            Status::Active
        },
        disabled,
        proxy_url,
        created_at: Some(ctx.now),
        updated_at: Some(ctx.now),
        ..Auth::default()
    };
    for key in [ATTRIBUTE_SOURCE, ATTRIBUTE_PATH] {
        auth.attributes.insert(key.to_owned(), path_str.to_owned());
    }
    auth.attributes.insert(
        ATTRIBUTE_SOURCE_BACKEND.to_owned(),
        AUTH_SOURCE_FILE.to_owned(),
    );
    apply_auth_priority_metadata(&mut auth, &metadata);
    auth.metadata = metadata;
    let metadata = auth.metadata.clone();
    apply_auth_weight_metadata(&mut auth, &metadata)
        .map_err(|err| SynthesisError::new(format!("invalid auth weight in {base}: {err}")))?;
    if let Some(note) = note {
        auth.attributes.insert("note".to_owned(), note);
    }
    apply_custom_headers_from_metadata(&mut auth);
    set_model_aliases_attribute(&mut auth, per_account_aliases);
    apply_auth_excluded_models_meta(
        &mut auth,
        &ctx.oauth_excluded_models,
        &per_account_excluded,
        AUTH_KIND_OAUTH,
    );
    if let Some(plan_type) = plan_type {
        auth.attributes.insert("plan_type".to_owned(), plan_type);
    }
    Ok(Some(auth))
}

/// Sets the attributes a file credential takes from the config again, as
/// synthesizing its file with `ctx` would set them: `excluded_models` and
/// `excluded_models_hash`, from the account's own list in its metadata and
/// the provider's `oauth-excluded-models`, and `auth_kind`. Returns whether
/// its attributes changed.
pub fn apply_config_attributes(ctx: &SynthesisContext, auth: &mut Auth) -> bool {
    let before = auth.attributes.clone();
    for key in [ATTRIBUTE_EXCLUDED_MODELS, ATTRIBUTE_EXCLUDED_MODELS_HASH] {
        auth.attributes.remove(key);
    }
    let per_account_excluded = extract_excluded_models(&auth.metadata);
    apply_auth_excluded_models_meta(
        auth,
        &ctx.oauth_excluded_models,
        &per_account_excluded,
        AUTH_KIND_OAUTH,
    );
    auth.attributes != before
}

/// One per-account model alias (upstream's `OAuthModelAlias`): requests for
/// `alias` go to the upstream model `name`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelAlias {
    /// The upstream model.
    pub name: String,
    /// The name clients use.
    pub alias: String,
    /// Whether the alias is listed alongside the model rather than in its
    /// place.
    pub fork: bool,
    /// The name shown in model lists, if any.
    pub display_name: String,
    /// Whether requests for the alias always map to the model.
    pub force_mapping: bool,
}

/// Stores `aliases`, sanitized, as `auth`'s `model_aliases` attribute in
/// JSON. Nothing is set when none survive.
pub fn set_model_aliases_attribute(auth: &mut Auth, aliases: Vec<ModelAlias>) {
    let aliases = sanitize_model_aliases(aliases);
    if aliases.is_empty() {
        return;
    }
    let items: Vec<String> = aliases.iter().map(alias_json).collect();
    auth.attributes.insert(
        ATTRIBUTE_MODEL_ALIASES.to_owned(),
        format!("[{}]", items.join(",")),
    );
}

/// Trims names and aliases and drops entries missing either, mapping a model
/// to itself, or repeating an earlier alias (ignoring case).
pub fn sanitize_model_aliases(aliases: Vec<ModelAlias>) -> Vec<ModelAlias> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for entry in aliases {
        let name = entry.name.trim();
        let alias = entry.alias.trim();
        if name.is_empty() || alias.is_empty() || equal_fold(name, alias) {
            continue;
        }
        if !seen.insert(open_ferry_translate::go::to_lower(alias)) {
            continue;
        }
        out.push(ModelAlias {
            name: name.to_owned(),
            alias: alias.to_owned(),
            fork: entry.fork,
            display_name: entry.display_name.trim().to_owned(),
            force_mapping: entry.force_mapping,
        });
    }
    out
}

/// One alias as Go's `json.Marshal` writes it.
fn alias_json(alias: &ModelAlias) -> String {
    use open_ferry_translate::go::json_string;
    let mut out = format!(
        "{{\"name\":{},\"alias\":{}",
        json_string(&alias.name),
        json_string(&alias.alias)
    );
    if alias.fork {
        out.push_str(",\"fork\":true");
    }
    if !alias.display_name.is_empty() {
        out.push_str(",\"display-name\":");
        out.push_str(&json_string(&alias.display_name));
    }
    if alias.force_mapping {
        out.push_str(",\"force-mapping\":true");
    }
    out.push('}');
    out
}

/// A credential file's model aliases, from `model_aliases` or else
/// `model-aliases`. A list Go couldn't decode into aliases yields none.
fn extract_model_aliases(metadata: &Map<String, Value>) -> Vec<ModelAlias> {
    let raw = metadata
        .get("model_aliases")
        .or_else(|| metadata.get("model-aliases"));
    match raw {
        Some(Value::Array(items)) => decode_model_aliases(items).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Go's `json.Unmarshal` into `[]OAuthModelAlias` of the list it marshaled
/// again: each key matching a field without regard to case decoded in
/// sorted order, `null` leaving the field as it was, and any value of the
/// wrong type failing the whole list.
fn decode_model_aliases(items: &[Value]) -> Option<Vec<ModelAlias>> {
    let text = |item: &Map<String, Value>, field: &str| {
        decode_field(
            remarshaled_fold_values(item, field),
            String::new(),
            |value| value.as_str().map(str::to_owned),
        )
    };
    let flag = |item: &Map<String, Value>, field: &str| {
        decode_field(remarshaled_fold_values(item, field), false, Value::as_bool)
    };
    items
        .iter()
        .map(|item| match item {
            Value::Null => Some(ModelAlias::default()),
            Value::Object(item) => Some(ModelAlias {
                name: text(item, "name")?,
                alias: text(item, "alias")?,
                fork: flag(item, "fork")?,
                display_name: text(item, "display-name")?,
                force_mapping: flag(item, "force-mapping")?,
            }),
            _ => None,
        })
        .collect()
}

/// A credential file's excluded models, from `excluded_models` or else
/// `excluded-models`: the strings in the list, trimmed, empty ones dropped.
fn extract_excluded_models(metadata: &Map<String, Value>) -> Vec<String> {
    let raw = metadata
        .get("excluded_models")
        .or_else(|| metadata.get("excluded-models"));
    let Some(Value::Array(items)) = raw else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A codex file's plan: its `plan_type`, else the plan claim of its ID
/// token, else nothing when it has neither.
fn codex_plan_type(metadata: &Map<String, Value>) -> Option<String> {
    if let Some(plan) = metadata
        .get("plan_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
    {
        return Some(plan.to_owned());
    }
    let token = metadata
        .get("id_token")
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())?;
    Some(id_token_plan_type(token).unwrap_or_else(|| DEFAULT_CODEX_PLAN_TYPE.to_owned()))
}

/// The `chatgpt_plan_type` claim of a codex ID token, trimmed, if it has a
/// non-empty one. The token's signature isn't checked.
fn id_token_plan_type(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let decoded = decode_jwt_segment_padded(payload)?;
    let claims: Value = serde_json::from_str(&String::from_utf8_lossy(&decoded)).ok()?;
    let claims = claims.as_object()?;
    let Some(Value::Object(info)) = fold_field(claims, "https://api.openai.com/auth") else {
        return None;
    };
    let Some(Value::String(plan)) = fold_field(info, "chatgpt_plan_type") else {
        return None;
    };
    let plan = plan.trim();
    (!plan.is_empty()).then(|| plan.to_owned())
}

/// Go's `filepath.Base` for the paths synthesized here.
fn base_name<'a>(path: &'a Path, path_str: &'a str) -> &'a str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use chrono::TimeZone;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn ctx(dir: &Path) -> SynthesisContext {
        SynthesisContext::new(
            dir,
            chrono::Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
        )
    }

    fn write(dir: &Path, name: &str, value: &Value) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        path
    }

    fn one(dir: &Path, value: Value) -> Auth {
        write(dir, "auth.json", &value);
        let mut auths = synthesize_file_auths(&ctx(dir));
        assert_eq!(auths.len(), 1);
        auths.remove(0)
    }

    #[test]
    fn empty_or_missing_dir_gives_nothing() {
        assert!(synthesize_file_auths(&ctx(Path::new(""))).is_empty());
        let dir = tempfile::tempdir().unwrap();
        assert!(synthesize_file_auths(&ctx(&dir.path().join("missing"))).is_empty());
    }

    #[test]
    fn valid_auth_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "claude-auth.json",
            &json!({
                "type": "claude",
                "email": "test@example.com",
                "proxy_url": "http://proxy.local",
                "prefix": "test-prefix",
                "headers": {" X-Test ": " value ", "X-Empty": "  "},
                "disable_cooling": true,
                "request_retry": 2,
            }),
        );
        let context = ctx(dir.path());
        let auths = synthesize_file_auths(&context);
        assert_eq!(auths.len(), 1);
        let auth = &auths[0];
        assert_eq!(auth.provider, "claude");
        assert_eq!(auth.label, "test@example.com");
        assert_eq!(auth.prefix, "test-prefix");
        assert_eq!(auth.proxy_url, "http://proxy.local");
        assert_eq!(auth.attribute("header:X-Test"), Some("value"));
        assert_eq!(auth.attribute("header:X-Empty"), None);
        assert_eq!(auth.metadata["disable_cooling"], Value::Bool(true));
        assert_eq!(auth.metadata["request_retry"].as_f64(), Some(2.0));
        assert_eq!(auth.status, Status::Active);
        assert_eq!(auth.id, "claude-auth.json");
        assert_eq!(auth.file_name, "claude-auth.json");
        assert_eq!(auth.created_at, Some(context.now));
        assert_eq!(auth.attribute(ATTRIBUTE_PATH), path.to_str());
        assert_eq!(
            auth.attribute(ATTRIBUTE_SOURCE_BACKEND),
            Some(AUTH_SOURCE_FILE)
        );
        assert_eq!(auth.attribute("auth_kind"), Some("oauth"));
        assert_eq!(auth.disable_cooling_override(), Some(true));
        assert_eq!(auth.request_retry_override(), Some(2));
    }

    #[test]
    fn ignores_gemini_files() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "gemini-auth.json",
            &json!({"type": "gemini", "email": "gemini@example.com"}),
        );
        write(
            dir.path(),
            "gemini-multi.json",
            &json!({
                "type": "GEMINI",
                "email": "multi@example.com",
                "project_id": "project-a, project-b",
                "priority": " 10 ",
            }),
        );
        write(
            dir.path(),
            "gemini-cli.json",
            &json!({"type": "gemini-cli"}),
        );
        assert!(synthesize_file_auths(&ctx(dir.path())).is_empty());
    }

    // No upstream test: a file as vertex_credentials.go's SaveTokenToFile
    // writes it.
    #[test]
    fn vertex_service_account_files() {
        let dir = tempfile::tempdir().unwrap();
        let service_account = json!({
            "type": "service_account",
            "project_id": "vertex-project",
            "private_key": "not-a-real-key",
            "client_email": "sa@vertex-project.iam.gserviceaccount.com",
        });
        let auth = one(
            dir.path(),
            json!({
                "service_account": service_account,
                "project_id": "vertex-project",
                "email": "sa@vertex-project.iam.gserviceaccount.com",
                "location": "europe-west4",
                "type": "vertex",
                "prefix": "team",
            }),
        );
        assert_eq!(auth.provider, "vertex");
        assert_eq!(auth.label, "sa@vertex-project.iam.gserviceaccount.com");
        assert_eq!(auth.prefix, "team");
        assert_eq!(auth.metadata["service_account"], service_account);
        assert_eq!(auth.metadata["location"], "europe-west4");
        assert_eq!(auth.attribute("auth_kind"), Some("oauth"));
        assert_eq!(auth.attribute("api_key"), None);
        let text = format!("{auth:?}");
        assert!(!text.contains("not-a-real-key"), "{text}");
    }

    #[test]
    fn skips_invalid_files_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("not-json.txt"), "text content").unwrap();
        fs::write(root.join("invalid.json"), "not valid json").unwrap();
        fs::write(root.join("empty.json"), "").unwrap();
        fs::write(root.join("null.json"), "null").unwrap();
        fs::write(
            root.join("no-type.json"),
            r#"{"email": "test@example.com"}"#,
        )
        .unwrap();
        fs::write(
            root.join("bad-weight.json"),
            r#"{"type":"claude","weight":1.5}"#,
        )
        .unwrap();
        fs::create_dir(root.join("subdir.json")).unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(
            root.join("nested").join("deep.json"),
            r#"{"type":"claude"}"#,
        )
        .unwrap();
        write(
            root,
            "valid.json",
            &json!({"type": "claude", "email": "valid@example.com"}),
        );
        let auths = synthesize_file_auths(&ctx(root));
        assert_eq!(auths.len(), 1);
        assert_eq!(auths[0].label, "valid@example.com");
    }

    #[test]
    fn id_is_relative_to_auth_dir() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "my-auth.json", &json!({"type": "claude"}));
        let auths = synthesize_file_auths(&ctx(dir.path()));
        assert_eq!(auths[0].id, "my-auth.json");
        assert_eq!(auths[0].label, "claude");

        // Without an auth directory the ID is the full path.
        let path = dir.path().join("my-auth.json");
        let data = fs::read(&path).unwrap();
        let auth = synthesize_auth_file(&ctx(Path::new(" ")), &path, &data)
            .unwrap()
            .unwrap();
        let want = path.to_str().unwrap();
        let want = if cfg!(windows) {
            open_ferry_translate::go::to_lower(want)
        } else {
            want.to_owned()
        };
        assert_eq!(auth.id, want);
    }

    #[test]
    fn prefix_validation() {
        for (prefix, want) in [
            ("myprefix", "myprefix"),
            ("/myprefix/", "myprefix"),
            ("  myprefix  ", "myprefix"),
            ("my/prefix", ""),
            ("", ""),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let auth = one(dir.path(), json!({"type": "claude", "prefix": prefix}));
            assert_eq!(auth.prefix, want, "{prefix:?}");
        }
    }

    #[test]
    fn priority_parsing() {
        for (priority, want) in [
            (json!(" 10 "), Some("10")),
            (json!(8), Some("8")),
            (json!("1x"), None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let auth = one(dir.path(), json!({"type": "claude", "priority": priority}));
            assert_eq!(auth.attribute("priority"), want, "{priority}");
        }
    }

    #[test]
    fn weight_parsing() {
        for (weight, want) in [
            (json!(5), Some("5")),
            (json!(" 3 "), Some("3")),
            (json!(0), Some("0")),
            (json!(-5), Some("0")),
            (json!(1_000_000), Some("1000000")),
            (json!(1.5), None),
            (json!(1_000_001), None),
            (json!("9223372036854775808"), None),
            (json!("heavy"), None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = write(
                dir.path(),
                "auth.json",
                &json!({"type": "claude", "weight": weight}),
            );
            let context = ctx(dir.path());
            let auths = synthesize_file_auths(&context);
            match want {
                Some(want) => {
                    assert_eq!(auths.len(), 1, "{weight}");
                    assert_eq!(auths[0].attribute("weight"), Some(want), "{weight}");
                }
                None => {
                    assert!(auths.is_empty(), "{weight}");
                    let data = fs::read(&path).unwrap();
                    let err = synthesize_auth_file(&context, &path, &data).unwrap_err();
                    assert!(
                        err.to_string().starts_with("invalid weight in auth.json: "),
                        "{err}"
                    );
                }
            }
        }
    }

    #[test]
    fn note_parsing() {
        for (note, want) in [
            (json!("hello world"), Some("hello world")),
            (json!("  trimmed note  "), Some("trimmed note")),
            (json!(""), None),
            (json!("   "), None),
            (json!(12345), None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let auth = one(dir.path(), json!({"type": "claude", "note": note}));
            assert_eq!(auth.attribute("note"), want, "{note}");
        }
    }

    #[test]
    fn oauth_excluded_models_merged() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "auth.json",
            &json!({"type": "claude", "excluded_models": ["custom-model", "MODEL-B"]}),
        );
        let mut context = ctx(dir.path());
        context.oauth_excluded_models = BTreeMap::from([(
            "claude".to_owned(),
            vec!["shared".to_owned(), "model-b".to_owned()],
        )]);
        let auths = synthesize_file_auths(&context);
        assert_eq!(
            auths[0].attribute("excluded_models"),
            Some("custom-model,model-b,shared")
        );
    }

    /// Not upstream's: the config's excluded models set again on a
    /// registered record match what synthesizing its file with that config
    /// gives, whatever the old config added.
    #[test]
    fn config_attributes_follow_the_config() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "auth.json",
            &json!({"type": "claude", "excluded_models": ["custom-model"]}),
        );
        let mut old = ctx(dir.path());
        old.oauth_excluded_models = BTreeMap::from([("claude".to_owned(), vec!["*".to_owned()])]);
        let mut auth = synthesize_file_auths(&old).remove(0);
        assert_eq!(auth.attribute("excluded_models"), Some("*,custom-model"));

        let mut new = ctx(dir.path());
        new.oauth_excluded_models = BTreeMap::from([("codex".to_owned(), vec!["*".to_owned()])]);
        assert!(apply_config_attributes(&new, &mut auth));
        let fresh = synthesize_file_auths(&new).remove(0);
        assert_eq!(auth.attributes, fresh.attributes);
        assert_eq!(auth.attribute("excluded_models"), Some("custom-model"));
        assert!(!apply_config_attributes(&new, &mut auth));

        auth.metadata.remove("excluded_models");
        assert!(apply_config_attributes(&new, &mut auth));
        assert_eq!(auth.attribute("excluded_models"), None);
        assert_eq!(auth.attribute("excluded_models_hash"), None);
        assert_eq!(auth.attribute("auth_kind"), Some("oauth"));
    }

    #[test]
    fn provider_oauth_excluded_models_merged() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "meta.json",
            &json!({
                "type": "meta",
                "auth_kind": "oauth",
                "api_key": "test-not-a-key",
                "excluded-models": ["muse-spark-1.2", 7, " "],
            }),
        );
        let mut context = ctx(dir.path());
        context.oauth_excluded_models =
            BTreeMap::from([("meta".to_owned(), vec!["muse-spark-1.1".to_owned()])]);
        let auths = synthesize_file_auths(&context);
        assert_eq!(auths[0].attribute("auth_kind"), Some("oauth"));
        assert_eq!(
            auths[0].attribute("excluded_models"),
            Some("muse-spark-1.1,muse-spark-1.2")
        );
    }

    #[test]
    fn oauth_model_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let auth = one(
            dir.path(),
            json!({
                "type": "codex",
                "email": "codex@example.com",
                "model_aliases": [
                    {"name": " gpt-5.3-codex-spark ", "alias": " gpt-5.5 "},
                    {"name": "gpt-5.3-codex-spark", "alias": "gpt-5.4", "fork": true},
                    {"name": "gpt-5.3-codex-spark", "alias": "gpt-5.5"},
                    {"name": "", "alias": "ignored"},
                ],
            }),
        );
        assert_eq!(
            auth.attribute("model_aliases"),
            Some(
                r#"[{"name":"gpt-5.3-codex-spark","alias":"gpt-5.5"},{"name":"gpt-5.3-codex-spark","alias":"gpt-5.4","fork":true}]"#
            )
        );
    }

    #[test]
    fn model_alias_decoding_matches_go() {
        let decode = |value: Value| extract_model_aliases(value.as_object().unwrap());
        // Go marshals the list again, sorting keys, and decodes each key in
        // turn: "name" comes after "Name", and a null leaves the name as it was.
        for aliases in [
            json!([{"name": "upstream", "Name": "other", "alias": "public"}]),
            json!([{"Name": "other", "name": "upstream", "alias": "public"}]),
            json!([{"name": "upstream", "Name": null, "alias": "public"}]),
            json!([{"Name": "upstream", "name": null, "alias": "public"}]),
        ] {
            let decoded = decode(json!({"model_aliases": aliases}));
            assert_eq!(decoded.len(), 1, "{decoded:?}");
            assert_eq!(decoded[0].name, "upstream", "{decoded:?}");
        }
        // A value of the wrong type under any of the keys fails the list.
        for aliases in [
            json!([{"name": 42, "Name": "upstream", "alias": "public"}]),
            json!([{"Name": 42, "name": "upstream", "alias": "public"}]),
        ] {
            assert!(decode(json!({"model_aliases": aliases})).is_empty());
        }
        // Case-insensitive fields, nulls, legacy key, extra fields ignored.
        let aliases = decode(json!({"model-aliases": [
            {"NAME": "m", "Alias": "a", "Display-Name": " Shown ", "force-mapping": true, "x": 1},
            null,
            {"name": "M", "alias": "m"},
        ]}));
        assert_eq!(aliases.len(), 3);
        let mut auth = Auth::default();
        set_model_aliases_attribute(&mut auth, aliases);
        assert_eq!(
            auth.attribute("model_aliases"),
            Some(r#"[{"name":"m","alias":"a","display-name":"Shown","force-mapping":true}]"#)
        );
        // A value of the wrong type rejects the whole list.
        assert!(
            decode(json!({"model_aliases": [{"name": "m", "alias": "a"}, {"fork": "yes"}]}))
                .is_empty()
        );
        assert!(decode(json!({"model_aliases": [{"name": "m", "alias": "a"}, "x"]})).is_empty());
        assert!(decode(json!({"model_aliases": {"name": "m", "alias": "a"}})).is_empty());
        // A null canonical key hides the legacy one.
        assert!(
            decode(json!({"model_aliases": null, "model-aliases": [{"name": "m", "alias": "a"}]}))
                .is_empty()
        );
    }

    fn codex_jwt(plan_type: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let mut info = json!({"chatgpt_account_id": "acc-123"});
        if !plan_type.is_empty() {
            info["chatgpt_plan_type"] = Value::from(plan_type);
        }
        let claims = json!({"email": "user@example.com", "https://api.openai.com/auth": info});
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.")
    }

    #[test]
    fn codex_plan_type() {
        for (file, want) in [
            (json!({"type": "codex", "plan_type": "pro"}), Some("pro")),
            (
                json!({"type": "codex", "plan_type": " ", "id_token": codex_jwt("team")}),
                Some("team"),
            ),
            (
                json!({"type": "codex", "id_token": codex_jwt("")}),
                Some("free"),
            ),
            (
                json!({"type": "codex", "id_token": "not-a-jwt"}),
                Some("free"),
            ),
            (
                json!({"type": "codex", "id_token": "a.!!!.c"}),
                Some("free"),
            ),
            (json!({"type": "codex", "id_token": " "}), None),
            (json!({"type": "codex"}), None),
            (json!({"type": "claude", "plan_type": "pro"}), None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = write(dir.path(), "codex.json", &file);
            let data = fs::read(&path).unwrap();
            let auth = synthesize_auth_file(&ctx(dir.path()), &path, &data)
                .unwrap()
                .unwrap();
            assert_eq!(auth.attribute("plan_type"), want, "{file}");
        }
    }

    #[test]
    fn disabled_files_are_marked() {
        let dir = tempfile::tempdir().unwrap();
        let auth = one(
            dir.path(),
            json!({"type": "codex", "disabled": true, "proxy_url": " x "}),
        );
        assert!(auth.disabled);
        assert_eq!(auth.status, Status::Disabled);
        // Unlike the file store, the synthesizer keeps the proxy URL as is.
        assert_eq!(auth.proxy_url, " x ");
    }
}
