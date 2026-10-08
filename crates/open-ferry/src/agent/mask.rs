//! Keeping secrets out of what the commands print.
//!
//! Two layers:
//! - **Masking**: a value whose key names a secret (one holding `api-key`,
//!   `apikey`, `api_key`, `secret`, `token`, `password`, `passwd`,
//!   `authorization`, `cookie`, `private-key` or `access-key`, or named
//!   `username` or `credential`, as a TURN server's are; not
//!   `max-retry-credentials` or `credentials.concurrency`, which are
//!   counts, nor `use-max-completion-tokens`, a switch), and a value under
//!   a list such a key holds, is shown as the dashboard shows a client key:
//!   a few of its last characters, and of a long one its first three (see
//!   [`open_ferry_dashboard::mask_client_key`]). That goes for a number or
//!   a boolean as for a string, as the loader reads one as a string key. A header under `headers`
//!   is masked as the request log masks it. In any other string, a URL's
//!   user and password become `***`, its query's secret parameters are
//!   hidden, and email addresses are masked.
//! - **Scrubbing**: before anything is printed, each secret of the config
//!   (before and after the command), `MANAGEMENT_PASSWORD`, the key file
//!   and the credential files in the auth directory (their tokens, keys
//!   and cookies) is replaced by `[redacted]` wherever it still appears,
//!   but for the secrets the command was asked to show. One shorter than
//!   eight characters is replaced only as a whole word, with no letter or
//!   digit just before or after it, so it doesn't break up the words it
//!   is part of (a weak key can still hide a word that matches it). Only
//!   text is scrubbed: in JSON, its strings and keys, never a number, a
//!   boolean or its shape, so it stays valid JSON.

use std::path::{Path, PathBuf};

use open_ferry_core::config::save::backup_path;
use open_ferry_core::config::{AnyValue, Config};
use open_ferry_core::observe::mask::{
    is_credential_header, mask_emails, mask_header_value, mask_sensitive_query,
};
use open_ferry_core::observe::redact::{Policy, REDACTED, Secrets};
use open_ferry_dashboard::mask_client_key;
use serde_json::{Map, Value};

use super::Context;
use super::target::read_key_file;
use super::values::any_to_json;

/// The parts of a key's name that make its value a secret.
const SECRET_NAME_PARTS: [&str; 12] = [
    "api-key",
    "apikey",
    "api_key",
    "secret",
    "token",
    "password",
    "passwd",
    "authorization",
    "cookie",
    "private-key",
    "access-key",
    "private_key",
];

/// The settings whose names hold a part of [`SECRET_NAME_PARTS`] but
/// whose values are no secret: a switch.
const NOT_SECRET_NAMES: [&str; 1] = ["use-max-completion-tokens"];

/// The most of a file read for its secrets.
const FILE_LIMIT: u64 = 16 * 1024 * 1024;

/// The most of a credential file read for its secrets.
const CREDENTIAL_LIMIT: u64 = 1024 * 1024;

/// The most credential files read for their secrets.
const CREDENTIAL_FILES: usize = 256;

/// The fields of a credential file (a sign-in's tokens, a service account's
/// key, a session) that make a file one, at any depth to
/// [`CREDENTIAL_DEPTH`] (a file nested deeper isn't searched, and is
/// refused): their names without case or separators, so `access_token`,
/// `accessToken` and `access-token` alike.
const CREDENTIAL_FIELDS: [&str; 7] = [
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "privatekey",
    "clientsecret",
    "tokens",
    "sessionkey",
];

/// How deep in a file's mappings and lists a credential field is looked
/// for: a file with a mapping or a list nested deeper is refused, as too
/// deeply nested to check.
const CREDENTIAL_DEPTH: usize = 32;

/// Whether a value under the key `name` is a secret.
pub(crate) fn is_secret_name(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    if NOT_SECRET_NAMES.contains(&lower.as_str()) {
        return false;
    }
    lower == "username"
        || lower == "credential"
        || SECRET_NAME_PARTS.iter().any(|part| lower.contains(part))
}

/// The text of a number or a boolean `value`, as the loader reads one
/// where it takes a string; `None` for any other value.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// Where a value sits: the key nearest above it, and whether that key's
/// mapping is a `headers` one.
#[derive(Clone, Copy)]
struct Place<'a> {
    key: Option<&'a str>,
    header: bool,
}

/// The place of the value at `parts`.
fn place_of(parts: &[String]) -> Place<'_> {
    let key = parts.last().map(String::as_str);
    let header = parts.len() >= 2
        && parts
            .get(parts.len() - 2)
            .is_some_and(|parent| parent.eq_ignore_ascii_case("headers"));
    Place { key, header }
}

/// `value`, which sits at `parts` in the config, masked.
pub(crate) fn mask_at(parts: &[String], value: &Value) -> Value {
    mask_value(value, place_of(parts))
}

/// What a value read from a file or standard input shows as, wherever a
/// change shows it: one marker, whatever the value holds, so nothing of it
/// shows, not its keys, its shape or its length.
pub(crate) const FROM_A_FILE: &str = "[redacted]";

/// The whole config `root`, masked.
pub(crate) fn mask_tree(root: &Value) -> Value {
    mask_value(
        root,
        Place {
            key: None,
            header: false,
        },
    )
}

fn mask_value(value: &Value, place: Place<'_>) -> Value {
    match value {
        Value::Object(map) => {
            let headers = place
                .key
                .is_some_and(|key| key.eq_ignore_ascii_case("headers"));
            Value::Object(
                map.iter()
                    .map(|(key, child)| {
                        let place = Place {
                            key: Some(key),
                            header: headers,
                        };
                        (mask_plain(key), mask_value(child, place))
                    })
                    .collect(),
            )
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| mask_value(item, place)).collect())
        }
        Value::String(text) => Value::String(mask_string(text, place)),
        // A number or a boolean is a string to the loader where it takes
        // one, so a key can be `true` or `12345`.
        other => match scalar_text(other) {
            Some(text) if place.key.is_some_and(is_secret_name) => {
                Value::String(mask_client_key(&text))
            }
            _ => other.clone(),
        },
    }
}

fn mask_string(text: &str, place: Place<'_>) -> String {
    let Some(key) = place.key else {
        return mask_plain(text);
    };
    if place.header && is_credential_header(key) {
        return mask_header_value(key, text);
    }
    if is_secret_name(key) {
        return if text.is_empty() {
            String::new()
        } else {
            mask_client_key(text)
        };
    }
    mask_plain(text)
}

/// `text`, which isn't a secret by its key, with what may still be one in
/// it masked: a URL's user and password, its query's secret parameters,
/// and email addresses.
pub(crate) fn mask_plain(text: &str) -> String {
    let text = mask_url(text);
    mask_emails(&text).into_owned()
}

/// `text` with the user and password of a URL in it, at its start, made
/// `***`, and the secret parameters of its query hidden.
fn mask_url(text: &str) -> String {
    let Some(scheme_end) = text.find("://") else {
        return text.to_owned();
    };
    let (scheme, rest) = text.split_at(scheme_end + 3);
    if scheme.contains(char::is_whitespace) {
        return text.to_owned();
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let authority = match authority.rfind('@') {
        Some(at) => format!("***{}", authority.get(at..).unwrap_or_default()),
        None => authority.to_owned(),
    };
    let tail = match tail.split_once('?') {
        Some((path, query)) => {
            let (query, fragment) = match query.split_once('#') {
                Some((query, fragment)) => (query, Some(fragment)),
                None => (query, None),
            };
            let mut out = format!("{path}?{}", mask_sensitive_query(query));
            if let Some(fragment) = fragment {
                out.push('#');
                out.push_str(fragment);
            }
            out
        }
        None => tail.to_owned(),
    };
    format!("{scheme}{authority}{tail}")
}

/// Adds the secrets in `value` to `secrets`: each value whose key names
/// a secret (a number or a boolean too), each credential header, and the
/// user and password of each URL.
pub(crate) fn collect_secrets(value: &Value, secrets: &mut Secrets) {
    collect(
        value,
        Place {
            key: None,
            header: false,
        },
        secrets,
    );
}

/// Whether `value`, set at `parts`, holds a secret: a value that would be
/// masked by its key.
pub(crate) fn holds_secret(parts: &[String], value: &Value) -> bool {
    let mut secrets = Secrets::new();
    collect(value, place_of(parts), &mut secrets);
    !secrets.is_empty()
}

fn collect(value: &Value, place: Place<'_>, secrets: &mut Secrets) {
    match value {
        Value::Object(map) => {
            let headers = place
                .key
                .is_some_and(|key| key.eq_ignore_ascii_case("headers"));
            for (key, child) in map {
                let place = Place {
                    key: Some(key),
                    header: headers,
                };
                collect(child, place, secrets);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect(item, place, secrets);
            }
        }
        Value::String(text) => match place.key {
            Some(key) if place.header && is_credential_header(key) => {
                secrets.add_header(key, text.as_bytes());
            }
            Some(key) if is_secret_name(key) => secrets.add(text),
            _ if text.contains("://") => secrets.add_url(text),
            _ => {}
        },
        other => {
            if let Some(text) = scalar_text(other).filter(|_| place.key.is_some_and(is_secret_name))
            {
                secrets.add(&text);
            }
        }
    }
}

/// The secrets of the YAML file at `path`, when it can be read.
fn file_secrets(path: &Path, secrets: &mut Secrets) {
    limited_file_secrets(path, FILE_LIMIT, secrets);
}

/// Adds the secrets of the YAML or JSON file at `path`, read up to `limit`
/// bytes, to `secrets`.
fn limited_file_secrets(path: &Path, limit: u64, secrets: &mut Secrets) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let mut data = Vec::new();
    if std::io::Read::read_to_end(&mut std::io::Read::take(file, limit), &mut data).is_err() {
        return;
    }
    let text = String::from_utf8_lossy(&data);
    if let Ok(value) = AnyValue::parse_yaml(&text) {
        collect_secrets(&any_to_json(&value), secrets);
    }
}

/// The auth directories of the config at `ctx.path`, where its credential
/// files are kept: the one it sets (or the default when it sets none or
/// doesn't load), and a relative one both from the working directory and
/// from the config's directory. In tests, only those under the temporary
/// directory, so a test never looks at a real one.
pub(crate) fn auth_dirs(ctx: &Context) -> Vec<PathBuf> {
    let config = std::fs::read(&ctx.path)
        .ok()
        .and_then(|data| Config::load_bytes(&data).ok())
        .unwrap_or_default();
    let Ok(dir) = config.resolve_auth_dir() else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    if dir.is_absolute() {
        dirs.push(dir);
    } else {
        if let Ok(cwd) = std::env::current_dir() {
            dirs.push(cwd.join(&dir));
        }
        if let Some(parent) = ctx.path.parent() {
            dirs.push(parent.join(&dir));
        }
    }
    let temp = std::env::temp_dir();
    dirs.retain(|dir| !cfg!(test) || dir.starts_with(&temp));
    dirs
}

/// Why a file a value would be read from is refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    /// It is a credential file: it holds this (named as the file names
    /// it, never a value of the file).
    Holds(String),
    /// It nests mappings or lists deeper than [`CREDENTIAL_DEPTH`], or
    /// deeper than JSON or YAML is read to, so it can't be checked.
    TooDeep,
    /// It has a mapping with a key that isn't text, at any depth, which
    /// has no JSON form, so what it holds can't be checked.
    KeyNotText,
}

impl Mark {
    /// What the file is, after its path.
    pub(crate) fn why(&self) -> String {
        match self {
            Self::Holds(mark) => format!("is a credential file: it holds {mark}"),
            Self::TooDeep => format!(
                "is too deeply nested to check for a sign-in's tokens or a key (more than {CREDENTIAL_DEPTH} levels)"
            ),
            Self::KeyNotText => {
                "has a mapping key that isn't text (a number, say), so it can't be checked for a sign-in's tokens or a key".to_owned()
            }
        }
    }
}

/// Mappings or lists nested deeper than a file is checked to.
struct TooDeep;

/// What makes `text` a file a value isn't read from, when it is one: a
/// PEM block, as a private key or a certificate is kept in; or, in JSON or
/// YAML, a field of [`CREDENTIAL_FIELDS`] that is set, at any depth to
/// [`CREDENTIAL_DEPTH`], named as the file names it; or mappings or lists
/// nested deeper than that, or a mapping with a key that isn't text, at
/// any depth, which aren't checked. Never a value of the file.
pub(crate) fn credential_mark(text: &str) -> Option<Mark> {
    if has_pem_block(text) {
        return Some(Mark::Holds(
            "a PEM block, as a private key or a certificate is kept in".to_owned(),
        ));
    }
    let value = match serde_json::from_str::<Value>(text) {
        Ok(value) => value,
        Err(json) => match AnyValue::parse_yaml(text) {
            // Such a mapping has no JSON form: what it holds, at any
            // depth, would be lost to the check.
            Ok(value) if has_key_not_text(&value) => return Some(Mark::KeyNotText),
            Ok(value) => any_to_json(&value),
            // Text that is neither is no structured file, but for one
            // nested deeper than either reads.
            Err(yaml) => {
                let too_deep = json.to_string().contains("recursion limit")
                    || yaml.to_string().contains("exceeded max depth");
                return too_deep.then_some(Mark::TooDeep);
            }
        },
    };
    match credential_field(&value, 0) {
        Ok(Some(name)) => Some(Mark::Holds(format!(
            "the field {name}, as a sign-in's tokens or a key are kept in"
        ))),
        Ok(None) => None,
        Err(TooDeep) => Some(Mark::TooDeep),
    }
}

/// Whether `value` has a mapping with a key that isn't text, at any
/// depth. (YAML is read to a bounded depth, so this recursion is too.)
fn has_key_not_text(value: &AnyValue) -> bool {
    match value {
        AnyValue::AnyMap => true,
        AnyValue::Seq(items) => items.iter().any(has_key_not_text),
        AnyValue::Map(entries) => entries.values().any(has_key_not_text),
        _ => false,
    }
}

/// `name` without case or separators: `access_token`, `accessToken` and
/// `access-token` are all `accesstoken`.
fn bare_name(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '_' | '-' | '.' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

/// The name of the first field of [`CREDENTIAL_FIELDS`] set in `value`,
/// which is `depth` levels down in a file; [`TooDeep`] for a mapping or a
/// list deeper than [`CREDENTIAL_DEPTH`].
fn credential_field(value: &Value, depth: usize) -> Result<Option<String>, TooDeep> {
    let children: Vec<&Value> = match value {
        Value::Object(_) | Value::Array(_) if depth > CREDENTIAL_DEPTH => return Err(TooDeep),
        Value::Object(map) => {
            let field = map.iter().find(|(key, child)| {
                !child.is_null() && CREDENTIAL_FIELDS.contains(&bare_name(key).as_str())
            });
            if let Some((key, _)) = field {
                return Ok(Some(key.clone()));
            }
            map.values().collect()
        }
        Value::Array(items) => items.iter().collect(),
        _ => return Ok(None),
    };
    for child in children {
        if let Some(name) = credential_field(child, depth + 1)? {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// Whether `text` holds a PEM block: `-----BEGIN `, a label of capitals,
/// digits and spaces, and `-----`, as `-----BEGIN PRIVATE KEY-----`.
fn has_pem_block(text: &str) -> bool {
    const BEGIN: &str = "-----BEGIN ";
    text.match_indices(BEGIN).any(|(at, _)| {
        let rest = text.get(at + BEGIN.len()..).unwrap_or_default();
        rest.find("-----").is_some_and(|end| {
            end > 0
                && end <= 64
                && rest.get(..end).is_some_and(|label| {
                    label.bytes().all(|byte| {
                        byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b' '
                    })
                })
        })
    })
}

/// Adds the secrets of the credential files in `ctx`'s auth directories,
/// up to [`CREDENTIAL_FILES`] of them, to `secrets`.
fn credential_secrets(ctx: &Context, secrets: &mut Secrets) {
    let mut read = 0;
    for dir in auth_dirs(ctx) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if read >= CREDENTIAL_FILES {
                return;
            }
            let path = entry.path();
            let json = path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
            if json && entry.file_type().is_ok_and(|kind| kind.is_file()) {
                limited_file_secrets(&path, CREDENTIAL_LIMIT, secrets);
                read += 1;
            }
        }
    }
}

/// The secrets a command must not print unasked: those of the config and
/// its backup, `MANAGEMENT_PASSWORD`, the key file, and the credential
/// files in the auth directory.
pub(crate) fn known_secrets(ctx: &Context) -> Secrets {
    let mut secrets = Secrets::new();
    file_secrets(&ctx.path, &mut secrets);
    file_secrets(&backup_path(&ctx.path), &mut secrets);
    credential_secrets(ctx, &mut secrets);
    if let Some(password) = &ctx.env.password {
        secrets.add(password);
    }
    for file in [ctx.key_file.as_deref(), ctx.env.key_file.as_deref()]
        .into_iter()
        .flatten()
    {
        if let Ok(key) = read_key_file(file) {
            secrets.add(&key);
        }
    }
    secrets
}

/// The shortest secret the scrub hides wherever it appears. A shorter one
/// could be an ordinary word, or part of one, so it is hidden only as a
/// whole word ([`hide_words`]).
const WHOLE_WORD_BELOW: usize = 8;

/// Replaces the secrets in what a command prints.
pub(crate) struct Scrub {
    secrets: Secrets,
    /// The forms of the secrets shorter than [`WHOLE_WORD_BELOW`], longest
    /// first: each as it is, and escaped as in a JSON string where that
    /// differs.
    short: Vec<String>,
}

impl Scrub {
    /// Of `secrets`.
    fn of(secrets: Secrets) -> Self {
        let mut short: Vec<String> = Vec::new();
        for secret in secrets
            .iter()
            .filter(|secret| secret.len() < WHOLE_WORD_BELOW)
        {
            let escaped = serde_json::to_string(secret).unwrap_or_default();
            let escaped = escaped
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .unwrap_or(secret);
            for form in [secret, escaped] {
                if !short.iter().any(|kept| kept == form) {
                    short.push(form.to_owned());
                }
            }
        }
        short.sort_by_key(|form| std::cmp::Reverse(form.len()));
        Self { secrets, short }
    }

    /// Of the secrets in `trees`, configs as JSON.
    pub(crate) fn of_trees(trees: &[&Value]) -> Self {
        let mut secrets = Secrets::new();
        for tree in trees {
            collect_secrets(tree, &mut secrets);
        }
        Self::of(secrets)
    }

    /// Of `secrets`, all but `reveal`.
    pub(crate) fn new(secrets: &Secrets, reveal: &[String]) -> Self {
        let reveal: Vec<&str> = reveal.iter().map(|secret| secret.trim()).collect();
        Self::of(
            secrets
                .iter()
                .filter(|secret| !reveal.contains(secret))
                .collect(),
        )
    }

    /// `text`, scrubbed: each secret of eight bytes or more wherever it
    /// appears, then each shorter one as a whole word.
    pub(crate) fn text(&self, text: String) -> String {
        let text = self.secrets.text(text, Policy::Client);
        match hide_words(&text, &self.short) {
            Some(hidden) => hidden,
            None => text,
        }
    }

    /// Each string and key of `value`, scrubbed.
    pub(crate) fn json(&self, value: Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|item| self.json(item)).collect())
            }
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(key, value)| (self.text(key), self.json(value)))
                    .collect::<Map<String, Value>>(),
            ),
            other => other,
        }
    }
}

/// `text` with each copy of each of `forms`, longest first, that is a
/// whole word replaced by [`REDACTED`]: one with no letter or digit just
/// before or after it. `None` when it has none.
fn hide_words(text: &str, forms: &[String]) -> Option<String> {
    if forms.is_empty() {
        return None;
    }
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut previous: Option<char> = None;
    for (at, c) in text.char_indices() {
        if at >= copied && previous.is_none_or(|before| !before.is_alphanumeric()) {
            let rest = text.get(at..).unwrap_or_default();
            let found = forms.iter().find(|form| {
                rest.strip_prefix(form.as_str()).is_some_and(|after| {
                    after
                        .chars()
                        .next()
                        .is_none_or(|next| !next.is_alphanumeric())
                })
            });
            if let Some(form) = found {
                let out = out.get_or_insert_with(|| String::with_capacity(text.len()));
                out.push_str(text.get(copied..at).unwrap_or_default());
                out.push_str(REDACTED);
                copied = at + form.len();
            }
        }
        previous = Some(c);
    }
    out.map(|mut out| {
        out.push_str(text.get(copied..).unwrap_or_default());
        out
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Not upstream's: a value is masked by the key nearest above it, a list
    // by the key that holds it, a header as the request log masks it.
    #[test]
    fn masks_by_key() {
        let key = "sk-0123456789abcdefghijklmnopqrstuvwxyz";
        let tree = json!({
            "access": {"api-keys": [key, "short-key-123"]},
            "management": {"secret-key": "a-management-secret", "allow-remote": false},
            "api-keys": {"codex": [{"api-key": key, "base-url": "https://user:pw@example.com/v1?key=abcdef&x=1", "headers": {"Authorization": "Bearer abcdefghijklmnop", "X-Plain": "visible"}}]},
            "proxy-url": "socks5://name:pass@127.0.0.1:1080",
            "note": "mail me at someone@example.com",
            "ice": [{"username": "turn-user", "credential": "turn-secret"}],
            "port": 8317,
        });
        let masked = mask_tree(&tree);
        assert_eq!(masked["access"]["api-keys"], json!(["sk-...wxyz", "...23"]));
        assert_eq!(masked["management"]["secret-key"], json!("...cret"));
        assert_eq!(masked["management"]["allow-remote"], json!(false));
        let codex = &masked["api-keys"]["codex"][0];
        assert_eq!(codex["api-key"], json!("sk-...wxyz"));
        let base_url = codex["base-url"].as_str().unwrap();
        assert!(
            base_url.starts_with("https://***@example.com/v1?key="),
            "{base_url}"
        );
        assert!(!base_url.contains("abcdef") && !base_url.contains("pw"));
        assert!(
            !codex["headers"]["Authorization"]
                .as_str()
                .unwrap()
                .contains("abcdefghijklmnop")
        );
        assert_eq!(codex["headers"]["X-Plain"], json!("visible"));
        assert_eq!(masked["proxy-url"], json!("socks5://***@127.0.0.1:1080"));
        assert!(
            !masked["note"]
                .as_str()
                .unwrap()
                .contains("someone@example.com")
        );
        assert_eq!(masked["ice"][0]["username"], json!("...er"));
        assert_eq!(masked["ice"][0]["credential"], json!("...et"));
        let counts = mask_tree(
            &json!({"routing": {"retry": {"max-retry-credentials": 2}}, "credentials": {"concurrency": 3}}),
        );
        assert_eq!(
            counts["routing"]["retry"]["max-retry-credentials"],
            json!(2)
        );
        assert_eq!(counts["credentials"]["concurrency"], json!(3));
        // A number or a boolean under a secret's key is masked, as the
        // loader reads it as a string; a switch elsewhere shows.
        let scalars = mask_tree(&json!({
            "access": {"api-keys": [true, 1234567890123_u64, false]},
            "management": {"secret-key": 98765432109_u64, "allow-remote": true},
            "api-keys": {"openai-compatibility": [{"models": [{"use-max-completion-tokens": true}]}]},
        }));
        assert_eq!(
            scalars["access"]["api-keys"],
            json!(["...", "...23", "..."])
        );
        assert_eq!(scalars["management"]["secret-key"], json!("...09"));
        assert_eq!(scalars["management"]["allow-remote"], json!(true));
        assert_eq!(
            scalars["api-keys"]["openai-compatibility"][0]["models"][0]["use-max-completion-tokens"],
            json!(true)
        );
        assert_eq!(masked["port"], json!(8317));
        assert!(!masked.to_string().contains(key));
        assert_eq!(
            mask_at(&["management".into(), "secret-key".into()], &json!("")),
            json!("")
        );
    }

    /// What `text` holds that makes it a credential file, if anything.
    fn holds(text: &str) -> Option<String> {
        match credential_mark(text) {
            Some(Mark::Holds(mark)) => Some(mark),
            Some(mark @ (Mark::TooDeep | Mark::KeyNotText)) => panic!("{mark:?}: {text}"),
            None => None,
        }
    }

    // Not upstream's: a credential file is told by a PEM block, or by a
    // sign-in's or a key's field at any depth to the limit, whatever its
    // case and separators; the mark names the field, never a value. A file
    // nested deeper than the limit is refused as too deep to check.
    #[test]
    fn finds_credential_files() {
        let value = "placeholder-value-0123456789";
        let pem = "-----BEGIN PRIVATE KEY-----\nplaceholder\n-----END PRIVATE KEY-----\n";
        for (text, field) in [
            // A sign-in's tokens at the top level, as a credential file has.
            (json!({"type": "gemini", "access_token": value}).to_string(), "access_token"),
            // Codex's auth.json: under `tokens`.
            (json!({"OPENAI_API_KEY": null, "tokens": {"id_token": value}}).to_string(), "tokens"),
            // Claude Code's: camelCase, under a mapping of its own.
            (json!({"claudeAiOauth": {"accessToken": value, "expiresAt": 1}}).to_string(), "accessToken"),
            (json!({"a": {"refreshToken": value}}).to_string(), "refreshToken"),
            (json!({"a": {"idToken": value}}).to_string(), "idToken"),
            (json!({"a": {"clientSecret": value}}).to_string(), "clientSecret"),
            (json!({"a": {"session_key": value}}).to_string(), "session_key"),
            (json!({"a": {"sessionKey": value}}).to_string(), "sessionKey"),
            (json!({"a": {"Access-Token": value}}).to_string(), "Access-Token"),
            // A service account, nested and in a list.
            (json!({"accounts": [{"credentials": {"type": "service_account", "privateKey": value}}]}).to_string(), "privateKey"),
            // Deep down.
            (json!({"a": {"b": {"c": {"d": {"e": {"f": {"g": {"h": {"refresh_token": value}}}}}}}}}).to_string(), "refresh_token"),
            // In YAML.
            (format!("oauth:\n  refresh-token: {value}\n"), "refresh-token"),
        ] {
            let mark = holds(&text).unwrap_or_else(|| panic!("not found: {text}"));
            assert!(mark.contains(field), "{mark}");
            assert!(!mark.contains(value), "{mark}");
        }
        // A PEM block, alone or in a JSON string.
        for text in [
            pem.to_owned(),
            json!({"key": pem}).to_string(),
            "-----BEGIN RSA PRIVATE KEY-----".to_owned(),
            "-----BEGIN CERTIFICATE-----".to_owned(),
        ] {
            let mark = holds(&text).unwrap();
            assert!(mark.starts_with("a PEM block"), "{mark}");
            assert!(!mark.contains("placeholder"));
        }
        // Not a secret's own file, a config, or a field that is unset.
        for text in [
            "sk-placeholder-key-0123456789\n".to_owned(),
            json!({"api-key": value, "base-url": "https://api.example.com"}).to_string(),
            json!([{"name": "example", "keys": [{"api-key": value}]}]).to_string(),
            json!({"use-max-completion-tokens": true, "max-tokens": 4}).to_string(),
            json!({"tokens": null, "access_token": null}).to_string(),
            "config-version: 8\nserver:\n  port: 1\n".to_owned(),
            "-----BEGIN lowercase-----".to_owned(),
            "a ----- b".to_owned(),
        ] {
            assert_eq!(credential_mark(&text), None, "{text}");
        }
        // At the limit, it is found; deeper, the file is too deep to
        // check, with a field or without, in a mapping or a list, and
        // deeper than JSON or YAML is read to.
        let nested =
            |levels: usize, inner: Value| (0..levels).fold(inner, |inner, _| json!({"a": inner}));
        let at_limit = nested(CREDENTIAL_DEPTH, json!({"access_token": value}));
        assert!(holds(&at_limit.to_string()).is_some());
        assert_eq!(
            credential_mark(&nested(CREDENTIAL_DEPTH, json!({"b": 1})).to_string()),
            None
        );
        for text in [
            nested(CREDENTIAL_DEPTH + 1, json!({"access_token": value})).to_string(),
            nested(CREDENTIAL_DEPTH + 1, json!({"b": 1})).to_string(),
            nested(CREDENTIAL_DEPTH, json!([[1]])).to_string(),
            format!("{}1{}", "[".repeat(300), "]".repeat(300)),
            format!("{}1{}", "{\"a\": ".repeat(300), "}".repeat(300)),
        ] {
            assert_eq!(credential_mark(&text), Some(Mark::TooDeep), "{text}");
        }
        assert!(Mark::TooDeep.why().contains("too deeply nested to check"));
    }

    // Not upstream's: a YAML mapping with a key that isn't text has no
    // JSON form, so a credential's field in it, or nesting too deep to
    // check below it, would be lost to the check: a file with one, at any
    // depth, is refused as one that can't be checked. A key that is text,
    // quoted or not, is checked as before.
    #[test]
    fn refuses_a_mapping_key_that_isnt_text() {
        let value = "placeholder-value-0123456789";
        let deep = format!(
            "{}1{}",
            "[".repeat(CREDENTIAL_DEPTH + 8),
            "]".repeat(CREDENTIAL_DEPTH + 8)
        );
        for text in [
            format!("1: one\naccess_token: {value}\n"),
            format!("tokens:\n  2: two\n  refresh-token: {value}\n"),
            format!("list:\n  - {{true: on, client_secret: {value}}}\n"),
            format!("a:\n  b:\n    ~: none\n    c: {value}\n"),
            format!("1.5: x\nb: {deep}\n"),
            format!("a: {{1: one, b: {deep}}}\n"),
        ] {
            assert_eq!(credential_mark(&text), Some(Mark::KeyNotText), "{text}");
        }
        assert!(
            Mark::KeyNotText
                .why()
                .contains("has a mapping key that isn't text")
        );
        assert_eq!(credential_mark(&format!("\"1\": one\nb: {value}\n")), None);
        assert!(holds(&format!("\"1\": one\naccess_token: {value}\n")).is_some());
    }

    // Not upstream's: the secrets found are what masking hides, and the
    // scrub hides them anywhere but those it may show.
    #[test]
    fn collects_and_scrubs() {
        let tree = json!({
            "management": {"secret-key": "management-secret-1"},
            "access": {"api-keys": ["client-key-0001", "client-key-0002"]},
            "proxy-url": "http://user:proxy-password@127.0.0.1:1",
            "routing": {"strategy": "round-robin"},
        });
        let mut secrets = Secrets::new();
        collect_secrets(&tree, &mut secrets);
        let found: Vec<&str> = secrets.iter().collect();
        assert!(found.contains(&"management-secret-1"));
        assert!(found.contains(&"client-key-0001"));
        assert!(found.iter().any(|secret| secret.contains("proxy-password")));
        assert!(!found.contains(&"round-robin"));

        let scrub = Scrub::new(&secrets, &["client-key-0002".to_owned()]);
        let out = scrub.json(json!({"a": "x management-secret-1 y", "b": ["client-key-0001"], "c": "client-key-0002"}));
        assert_eq!(
            out,
            json!({"a": "x [redacted] y", "b": ["[redacted]"], "c": "client-key-0002"})
        );
        assert_eq!(
            scrub.text("key client-key-0001".to_owned()),
            "key [redacted]"
        );

        assert!(holds_secret(
            &["management".into(), "secret-key".into()],
            &json!("x")
        ));
        assert!(holds_secret(
            &["api-keys".into(), "codex".into()],
            &json!([{"api-key": "k"}])
        ));
        assert!(!holds_secret(
            &["access".into(), "api-keys".into()],
            &json!([])
        ));
        assert!(!holds_secret(
            &["routing".into(), "strategy".into()],
            &json!("x")
        ));
        // A number or a boolean under a secret's key is one too, but not a
        // switch whose name only looks like one.
        for value in [json!([true]), json!([12345]), json!([false])] {
            assert!(holds_secret(&["access".into(), "api-keys".into()], &value));
        }
        assert!(holds_secret(
            &["management".into(), "secret-key".into()],
            &json!(true)
        ));
        assert!(!holds_secret(
            &["management".into(), "allow-remote".into()],
            &json!(true)
        ));
        assert!(!holds_secret(
            &["models".into(), "use-max-completion-tokens".into()],
            &json!(true)
        ));
        let mut found = Secrets::new();
        collect_secrets(&json!({"secret-key": 98765432109_u64}), &mut found);
        assert_eq!(found.iter().collect::<Vec<_>>(), ["98765432109"]);
    }

    // Not upstream's: a secret shorter than eight characters, a string, a
    // number or a boolean as its text, is scrubbed where it is a whole
    // word, as in a URL's path or in free text, and its JSON-escaped form
    // too; a word it is only part of is left as it is. In JSON only the
    // strings and keys are scrubbed, so the numbers and booleans, and the
    // shape, stay as they were. A secret the command was asked to show is
    // left alone.
    #[test]
    fn scrubs_short_secrets_as_whole_words() {
        let tree = json!({
            "access": {"api-keys": ["k3y9", 4242, true, "a\"b"]},
            "management": {"secret-key": "long-management-secret"},
        });
        let mut secrets = Secrets::new();
        collect_secrets(&tree, &mut secrets);
        let scrub = Scrub::new(&secrets, &[]);
        for (text, scrubbed) in [
            (
                "https://api.example.com/v1/k3y9/4242/true/models?n=4242",
                "https://api.example.com/v1/[redacted]/[redacted]/[redacted]/models?n=[redacted]",
            ),
            (
                "k3y9 and 4242 and true, not k3y9s, xk3y9, 42424, k3y9_4242 or untrue",
                "[redacted] and [redacted] and [redacted], not k3y9s, xk3y9, 42424, [redacted]_[redacted] or untrue",
            ),
            ("k3y9", "[redacted]"),
            ("xlong-management-secretx", "x[redacted]x"),
            ("{\"q\": \"a\\\"b\"}", "{\"q\": \"[redacted]\"}"),
            ("a\"b", "[redacted]"),
            ("nothing here", "nothing here"),
            ("", ""),
        ] {
            assert_eq!(scrub.text(text.to_owned()), scrubbed, "{text}");
        }
        let out = scrub.json(json!({
            "k3y9": [true, 4242, "true", "4242", false],
            "url": "https://api.example.com/v1/k3y9/",
            "port": 4242,
            "on": true,
        }));
        assert_eq!(
            out,
            json!({
                "[redacted]": [true, 4242, "[redacted]", "[redacted]", false],
                "url": "https://api.example.com/v1/[redacted]/",
                "port": 4242,
                "on": true,
            })
        );
        let printed = serde_json::to_string(&out).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&printed).unwrap(), out);

        let shown = Scrub::new(&secrets, &["k3y9".to_owned()]);
        assert_eq!(
            shown.text("k3y9 and 4242".to_owned()),
            "k3y9 and [redacted]"
        );
    }
}
