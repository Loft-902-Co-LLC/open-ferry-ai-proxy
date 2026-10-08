//! The settings a v8 config may hold, by path, for the tools that name a
//! setting: what [`known_v8_paths`] lists is what an edit at that path can
//! write. Not upstream's: `open-ferry config` refuses a path that isn't
//! here, and suggests the nearest one that is.
//!
//! The list comes from the tables the v8 checks use: every field of
//! upstream's `legacyConfig` and the structs it holds (see `schema`),
//! moved to its v8 path as the layout moves it, kept when its section is
//! one a v8 file may have, with the `api-keys` provider groups and the
//! sections that hold the moved settings.

use std::collections::BTreeMap;

use super::super::layout::V8_STRUCT_PATHS;
use super::super::save::v8_aliases;
use super::super::v8::{V8_KEY_FAMILIES, V8_PATHS, V8_SHARED_STRUCT_PATHS};
use super::schema::{CONFIG_LEGACY_CONFIG, Field, Type};
use super::validate::allowed_root;

/// What a known path holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnownKind {
    /// A section of settings: a mapping whose keys are listed too.
    Section,
    /// A setting: a value, or a mapping whose keys are the user's (such as
    /// `headers`), so a longer path under it is the user's to name.
    Value,
    /// A list, which is replaced whole: no path goes under it.
    List,
}

/// A setting or section a v8 config may hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownPath {
    /// Its keys, dotted, as `routing.quota.prefer`.
    pub path: String,
    /// What it holds.
    pub kind: KnownKind,
}

/// Every setting and section a v8 config may hold, sorted by path.
pub fn known_v8_paths() -> Vec<KnownPath> {
    let mut legacy = Vec::new();
    walk("", &CONFIG_LEGACY_CONFIG, &mut legacy);
    let mut known = BTreeMap::new();
    for (path, kind) in legacy {
        let Some(path) = v8_path(&path) else {
            continue;
        };
        let root = path.split('.').next().unwrap_or_default();
        if !allowed_root(root) {
            continue;
        }
        add_sections(&path, &mut known);
        known.insert(path, kind);
    }
    known.insert("api-keys".to_owned(), KnownKind::Section);
    for &(_, group) in V8_KEY_FAMILIES {
        known.insert(format!("api-keys.{group}"), KnownKind::List);
    }
    known.insert("config-version".to_owned(), KnownKind::Value);
    known
        .into_iter()
        .map(|(path, kind)| KnownPath { path, kind })
        .collect()
}

/// Whether `parts` names a setting or section a v8 config may hold, or a
/// key under a setting whose keys are the user's, of the `known` paths.
pub fn is_known_v8_path(known: &[KnownPath], parts: &[&str]) -> bool {
    let path = parts.join(".");
    known.iter().any(|entry| {
        entry.path == path
            || (entry.kind == KnownKind::Value
                && path
                    .strip_prefix(&entry.path)
                    .is_some_and(|rest| rest.starts_with('.')))
    })
}

/// Lists the fields of `ty` under `prefix`, each with what it holds.
fn walk(prefix: &str, ty: &Type, out: &mut Vec<(String, KnownKind)>) {
    for &(name, field) in ty.fields {
        let path = format!("{prefix}{name}");
        match field {
            Field::Leaf | Field::MapList(_) => out.push((path, KnownKind::Value)),
            Field::List(_) => out.push((path, KnownKind::List)),
            Field::Struct(inner) => {
                walk(&format!("{path}."), inner, out);
                out.push((path, KnownKind::Section));
            }
        }
    }
}

/// The v8 path of the legacy setting `path`, or `None` when the v8 layout
/// keeps it elsewhere (an API-key list, which becomes a provider group).
fn v8_path(path: &str) -> Option<String> {
    let root = path.split('.').next().unwrap_or_default();
    if V8_KEY_FAMILIES.iter().any(|&(old, _)| old == root) {
        return None;
    }
    let moves = V8_PATHS
        .iter()
        .chain(V8_STRUCT_PATHS)
        .chain(v8_aliases())
        .chain(V8_SHARED_STRUCT_PATHS);
    let mut best: Option<(&str, &str)> = None;
    for &(old, current) in moves {
        let matches = path == old
            || path
                .strip_prefix(old)
                .is_some_and(|rest| rest.starts_with('.'));
        if matches && best.is_none_or(|(longest, _)| old.len() > longest.len()) {
            best = Some((old, current));
        }
    }
    Some(match best {
        Some((old, current)) => format!("{current}{}", path.get(old.len()..).unwrap_or_default()),
        None => path.to_owned(),
    })
}

/// Adds the sections on the way to `path`.
fn add_sections(path: &str, known: &mut BTreeMap<String, KnownKind>) {
    let mut end = 0;
    while let Some(dot) = path.get(end..).and_then(|rest| rest.find('.')) {
        end += dot;
        if let Some(section) = path.get(..end) {
            known
                .entry(section.to_owned())
                .or_insert(KnownKind::Section);
        }
        end += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(known: &[KnownPath], path: &str) -> Option<KnownKind> {
        known
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.kind)
    }

    // Not upstream's: the paths come from the legacy fields, moved.
    #[test]
    fn lists_the_v8_paths() {
        let known = known_v8_paths();
        for (path, want) in [
            ("routing.strategy", KnownKind::Value),
            ("routing.quota.prefer", KnownKind::Value),
            ("routing.quota", KnownKind::Section),
            ("server.host", KnownKind::Value),
            ("server.port", KnownKind::Value),
            ("server.tls.enable", KnownKind::Value),
            ("server.tls", KnownKind::Section),
            ("management.secret-key", KnownKind::Value),
            ("management.allow-remote", KnownKind::Value),
            ("management.separate-address", KnownKind::Value),
            ("access.api-keys", KnownKind::Value),
            ("api-keys.codex", KnownKind::List),
            ("api-keys.openai-compatibility", KnownKind::List),
            ("api-keys", KnownKind::Section),
            ("observability.logs.debug", KnownKind::Value),
            ("claude-cli", KnownKind::List),
            ("config-version", KnownKind::Value),
        ] {
            assert_eq!(kind(&known, path), Some(want), "{path}");
        }
        for path in [
            "host",
            "port",
            "tls.enable",
            "remote-management.secret-key",
            "codex-api-key",
            "debug",
        ] {
            assert_eq!(kind(&known, path), None, "{path}");
        }
        assert!(known.windows(2).all(|pair| pair[0].path < pair[1].path));
    }

    // Not upstream's: a key under a setting whose keys are the user's is
    // known; one under a list or an unknown section isn't.
    #[test]
    fn checks_paths() {
        let known = known_v8_paths();
        assert!(is_known_v8_path(&known, &["routing", "strategy"]));
        assert!(is_known_v8_path(&known, &["routing"]));
        assert!(!is_known_v8_path(&known, &["routing", "stratgy"]));
        assert!(!is_known_v8_path(&known, &["api-keys", "codex", "0"]));
        assert!(!is_known_v8_path(&known, &["nope"]));
        assert!(!is_known_v8_path(&known, &[]));
    }
}
