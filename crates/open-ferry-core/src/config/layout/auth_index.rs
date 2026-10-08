// Ported from CLIProxyAPI internal/api/handlers/management/config_auth_index.go
// (injectV8APIKeyAuthIndexesLocked, yamlMapScalar, setMapScalar,
// yamlMapScalarPresent, yamlMapHeadersPresent, resolveInheritedScalar,
// resolveInheritedHeaders, normalizeModelPrefixHelper and
// formatCredentialDedupKey) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The `auth_index` a v8 config read shows on each API key.
//!
//! [`V8Document::set_api_key_auth_indexes`] finds, for each key of the
//! `api-keys` groups, the credential the config makes of it and sets the
//! key's `auth_index` to that credential's index, as upstream does before
//! it answers a v8 JSON read. An OpenAI-compatible group without keys gets
//! the index of its keyless credential on the group itself. A key whose
//! credential isn't found keeps what it has.
//!
//! Each provider finds its credentials as upstream does:
//! - Gemini and interactions keys by what makes a credential distinct: the
//!   key, the group's base URL, and the proxy URL, prefix and headers the
//!   key has or else inherits from its group.
//! - Vertex keys by the key and the group's base URL.
//! - Claude, Codex, xAI and Meta keys by their place among the keys the
//!   config loader keeps: every key for Claude, the keys of groups with a
//!   base URL for Codex and xAI, and the keys that aren't empty or
//!   `dca:` for Meta.
//! - OpenAI-compatible keys by their place in their group, among the
//!   groups with a base URL.
//!
//! Loading the config drops `auth_index` again when it expands the
//! `api-keys` groups, so a file holding what a read showed loads as it
//! would without it.
//!
//! Deviations from upstream: none.

use std::collections::{BTreeMap, HashMap};

use super::V8Document;
use crate::auth::Auth;
use crate::auth::synthesizer::format_sorted_headers;
use crate::config::Config;
use crate::config::decode::decode;
use crate::config::normalize::{normalize_headers, normalize_model_prefix};
use crate::config::yaml::{Kind, Node, find_map_key_index};

/// The key the index is shown under.
const AUTH_INDEX: &str = "auth_index";

/// The providers whose keys are found by their place, with the provider
/// name of their credentials.
const BY_PLACE: [&str; 4] = ["codex", "claude", "xai", "meta"];

impl V8Document {
    /// Sets the `auth_index` of each API key in the `api-keys` groups, and
    /// of each OpenAI-compatible group without keys, to the index of the
    /// credential it makes (upstream's `injectV8APIKeyAuthIndexesLocked`).
    /// `auths` are the credentials `config` makes, in the synthesizer's
    /// order, and `index_of` gives a credential's index: the one the
    /// running credential has, or else the one it derives.
    pub fn set_api_key_auth_indexes(
        &mut self,
        config: &Config,
        auths: &[Auth],
        index_of: impl Fn(&Auth) -> String,
    ) {
        if self.root.kind != Kind::Mapping {
            return;
        }
        let Some(api_keys) = value_mut(&mut self.root, "api-keys") else {
            return;
        };
        if api_keys.kind != Kind::Mapping {
            return;
        }
        let lookup = Lookup::new(config, auths, &index_of);
        for [name, groups] in api_keys.content.as_chunks_mut::<2>().0 {
            if groups.kind != Kind::Sequence {
                continue;
            }
            match name.value.trim() {
                "openai-compatibility" => compat_groups(groups, auths, &index_of),
                "claude" => claude_groups(groups, &lookup, &index_of),
                provider @ ("codex" | "xai") => {
                    based_groups(groups, provider, &lookup, &index_of);
                }
                "meta" => meta_groups(groups, &lookup, &index_of),
                provider => keyed_groups(groups, provider, &lookup),
            }
        }
    }
}

/// The credentials to look keys up in.
struct Lookup<'a> {
    /// Gemini, interactions and Vertex indexes, by provider and by what
    /// makes the key distinct; the last credential wins.
    distinct: HashMap<&'static str, HashMap<String, String>>,
    /// Claude, Codex, xAI and Meta credentials, by provider and by their
    /// `config_index`; the last credential wins.
    placed: HashMap<&'static str, HashMap<&'a str, &'a Auth>>,
}

impl<'a> Lookup<'a> {
    fn new(config: &Config, auths: &'a [Auth], index_of: &impl Fn(&Auth) -> String) -> Self {
        let config_index = |auth: &Auth, len: usize| {
            auth.attribute("config_index")
                .and_then(|index| index.parse::<usize>().ok())
                .filter(|&index| index < len)
        };
        let mut distinct: HashMap<&'static str, HashMap<String, String>> = HashMap::new();
        for (name, provider, keys) in [
            ("gemini", "gemini", &config.gemini_api_key),
            (
                "interactions",
                "gemini-interactions",
                &config.interactions_api_key,
            ),
        ] {
            let found = distinct.entry(name).or_default();
            for auth in auths.iter().filter(|auth| auth.provider == provider) {
                let Some(key) = config_index(auth, keys.len()).and_then(|index| keys.get(index))
                else {
                    continue;
                };
                let id = distinct_key(
                    &key.api_key,
                    &key.base_url,
                    &key.proxy_url,
                    &key.prefix,
                    &key.headers,
                );
                found.insert(id, index_of(auth));
            }
        }
        let vertex = distinct.entry("vertex").or_default();
        let keys = &config.vertex_api_key;
        for auth in auths.iter().filter(|auth| auth.provider == "vertex") {
            let Some(key) = config_index(auth, keys.len()).and_then(|index| keys.get(index)) else {
                continue;
            };
            let id = format!("{}|{}", key.api_key.trim(), key.base_url.trim());
            vertex.insert(id, index_of(auth));
        }

        let mut placed: HashMap<&'static str, HashMap<&'a str, &'a Auth>> = HashMap::new();
        for provider in BY_PLACE {
            let found = placed.entry(provider).or_default();
            for auth in auths.iter().filter(|auth| auth.provider == provider) {
                found.insert(auth.attribute("config_index").unwrap_or_default(), auth);
            }
        }
        Self { distinct, placed }
    }

    /// The credential at `place` among `provider`'s keys.
    fn placed(&self, provider: &str, place: usize) -> Option<&'a Auth> {
        let place = place.to_string();
        self.placed
            .get(provider)
            .and_then(|found| found.get(place.as_str()))
            .copied()
    }
}

/// The OpenAI-compatible groups: each group with a base URL is the
/// provider at the next `config_index`; its keys take its credentials in
/// turn, and a group without keys takes its keyless credential.
fn compat_groups(groups: &mut Node, auths: &[Auth], index_of: &impl Fn(&Auth) -> String) {
    let mut place = 0usize;
    for group in &mut groups.content {
        if group.kind != Kind::Mapping {
            continue;
        }
        if scalar(group, "base-url").is_empty() {
            continue;
        }
        let target = place.to_string();
        place += 1;
        let of_group = |auth: &&Auth| {
            auth.attribute("config_index").unwrap_or_default() == target
                && (auth.provider == "openai-compatibility"
                    || auth.provider.starts_with("openai-compatible-"))
        };
        let keyless = value(group, "keys")
            .is_none_or(|keys| keys.kind != Kind::Sequence || keys.content.is_empty());
        if keyless {
            let found = auths
                .iter()
                .filter(of_group)
                .find(|auth| auth.attribute("api_key").unwrap_or_default().is_empty());
            if let Some(auth) = found {
                set_found(group, &index_of(auth));
            }
            continue;
        }
        let group_auths: Vec<&Auth> = auths.iter().filter(of_group).collect();
        let Some(keys) = value_mut(group, "keys") else {
            continue;
        };
        for (at, key) in keys.content.iter_mut().enumerate() {
            if key.kind != Kind::Mapping {
                continue;
            }
            if let Some(auth) = group_auths.get(at) {
                set_found(key, &index_of(auth));
            }
        }
    }
}

/// The Claude groups: every key takes the next `config_index`, but a key
/// with neither a key nor a group base URL, which makes no credential, is
/// left alone.
fn claude_groups(groups: &mut Node, lookup: &Lookup<'_>, index_of: &impl Fn(&Auth) -> String) {
    let mut place = 0usize;
    for group in &mut groups.content {
        if group.kind != Kind::Mapping {
            continue;
        }
        let base_url = scalar(group, "base-url");
        let Some(keys) = sequence_mut(group, "keys") else {
            continue;
        };
        for key in &mut keys.content {
            if key.kind != Kind::Mapping {
                continue;
            }
            let target = place;
            place += 1;
            if scalar(key, "api-key").is_empty() && base_url.is_empty() {
                continue;
            }
            if let Some(auth) = lookup.placed("claude", target) {
                set_found(key, &index_of(auth));
            }
        }
    }
}

/// The Codex or xAI groups: the keys of a group with a base URL take the
/// next `config_index` each.
fn based_groups(
    groups: &mut Node,
    provider: &str,
    lookup: &Lookup<'_>,
    index_of: &impl Fn(&Auth) -> String,
) {
    let mut place = 0usize;
    for group in &mut groups.content {
        if group.kind != Kind::Mapping || scalar(group, "base-url").is_empty() {
            continue;
        }
        let Some(keys) = sequence_mut(group, "keys") else {
            continue;
        };
        for key in &mut keys.content {
            if key.kind != Kind::Mapping {
                continue;
            }
            let target = place;
            place += 1;
            if let Some(auth) = lookup.placed(provider, target) {
                set_found(key, &index_of(auth));
            }
        }
    }
}

/// The Meta groups: each key that isn't empty or a `dca:` key takes the
/// next `config_index`.
fn meta_groups(groups: &mut Node, lookup: &Lookup<'_>, index_of: &impl Fn(&Auth) -> String) {
    let mut place = 0usize;
    for group in &mut groups.content {
        if group.kind != Kind::Mapping {
            continue;
        }
        let Some(keys) = sequence_mut(group, "keys") else {
            continue;
        };
        for key in &mut keys.content {
            if key.kind != Kind::Mapping {
                continue;
            }
            let api_key = scalar(key, "api-key");
            if api_key.is_empty() || api_key.starts_with("dca:") {
                continue;
            }
            let target = place;
            place += 1;
            if let Some(auth) = lookup.placed("meta", target) {
                set_found(key, &index_of(auth));
            }
        }
    }
}

/// Any other provider's groups: a Vertex key found by its key and base
/// URL, a Gemini or interactions key by what makes it distinct. Other
/// providers find nothing.
fn keyed_groups(groups: &mut Node, provider: &str, lookup: &Lookup<'_>) {
    let found = lookup.distinct.get(provider);
    for group in &mut groups.content {
        if group.kind != Kind::Mapping {
            continue;
        }
        let base_url = scalar(group, "base-url");
        let group_proxy_url = present_scalar(group, "proxy-url");
        let group_prefix = present_scalar(group, "prefix");
        let group_headers = present_headers(group, "headers");
        let Some(keys) = sequence_mut(group, "keys") else {
            continue;
        };
        for key in &mut keys.content {
            if key.kind != Kind::Mapping {
                continue;
            }
            let api_key = scalar(key, "api-key");
            let id = if provider == "vertex" {
                format!("{api_key}|{base_url}")
            } else {
                let proxy_url = present_scalar(key, "proxy-url")
                    .or_else(|| group_proxy_url.clone())
                    .unwrap_or_default();
                let prefix = present_scalar(key, "prefix")
                    .or_else(|| group_prefix.clone())
                    .unwrap_or_default();
                let headers = present_headers(key, "headers")
                    .or_else(|| group_headers.clone())
                    .unwrap_or_default();
                distinct_key(&api_key, &base_url, &proxy_url, &prefix, &headers)
            };
            if let Some(index) = found.and_then(|found| found.get(&id)) {
                set_index(key, index);
            }
        }
    }
}

/// What makes an API key's credential distinct (upstream's
/// `formatCredentialDedupKey`).
fn distinct_key(
    key: &str,
    base_url: &str,
    proxy_url: &str,
    prefix: &str,
    headers: &BTreeMap<String, String>,
) -> String {
    format!(
        "{}\0{}\0{}\0{}\0{}",
        key.trim(),
        base_url.trim(),
        proxy_url.trim(),
        normalize_model_prefix(prefix),
        format_sorted_headers(&normalize_headers(headers)),
    )
}

/// The value of a mapping's first `key`.
fn value<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
    let index = find_map_key_index(node, key)?;
    node.content.get(index + 1)
}

/// [`value`], to change.
fn value_mut<'a>(node: &'a mut Node, key: &str) -> Option<&'a mut Node> {
    let index = find_map_key_index(node, key)?;
    node.content.get_mut(index + 1)
}

/// The sequence under a mapping's first `key`.
fn sequence_mut<'a>(node: &'a mut Node, key: &str) -> Option<&'a mut Node> {
    value_mut(node, key).filter(|value| value.kind == Kind::Sequence)
}

/// The trimmed text of a mapping's first `key`, whatever its kind; empty
/// when there is none (upstream's `yamlMapScalar`).
fn scalar(node: &Node, key: &str) -> String {
    value(node, key)
        .map(|value| value.value.trim().to_owned())
        .unwrap_or_default()
}

/// [`scalar`], or `None` when the key is missing or null (upstream's
/// `yamlMapScalarPresent`).
fn present_scalar(node: &Node, key: &str) -> Option<String> {
    let value = value(node, key)?;
    (value.tag != "!!null").then(|| value.value.trim().to_owned())
}

/// The headers under a mapping's first `key`, or `None` when the key is
/// missing, null or not a mapping of strings (upstream's
/// `yamlMapHeadersPresent`).
fn present_headers(node: &Node, key: &str) -> Option<BTreeMap<String, String>> {
    let value = value(node, key)?;
    if value.tag == "!!null" {
        return None;
    }
    decode(value).ok()
}

/// [`set_index`], unless `index` is empty.
fn set_found(node: &mut Node, index: &str) {
    if !index.is_empty() {
        set_index(node, index);
    }
}

/// Sets a mapping's `auth_index` to `index`, as a string, adding it when
/// it is missing (upstream's `setMapScalar`).
fn set_index(node: &mut Node, index: &str) {
    if node.kind != Kind::Mapping {
        return;
    }
    let scalar = Node::scalar("!!str", index);
    match value_mut(node, AUTH_INDEX) {
        Some(value) => *value = scalar,
        None => {
            node.content.push(Node::scalar("!!str", AUTH_INDEX));
            node.content.push(scalar);
        }
    }
}
