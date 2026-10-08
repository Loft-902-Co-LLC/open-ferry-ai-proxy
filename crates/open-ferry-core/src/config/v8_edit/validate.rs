// Ported from CLIProxyAPI internal/config/config_v8.go (ValidateV8Config,
// v8AllowedRoots) and internal/registry/catalog_config.go
// (CatalogSources.Validate) (v8.0.20, MIT), with the key checks of yaml.v3
// v3.0.1's strict decode (decode.go's mapping and mappingStruct with
// KnownFields).
// https://github.com/router-for-me/CLIProxyAPI

//! Upstream's check that a file is a valid v8 config, which each v8 edit
//! passes before it is written: the v8 layout without legacy fields,
//! known sections and API-key providers only, and, moved into the legacy
//! layout, no key that upstream's `legacyConfig` doesn't have, and
//! `models` sources that are http(s) URLs or absolute paths.
//!
//! Deviations from upstream:
//! - The strict decode checks keys: repeated keys, keys a struct doesn't
//!   have, and keys that aren't strings. It doesn't check the values'
//!   types, apart from the `models` section's, which
//!   [`super::super::Config::parse`] checks first for the settings it
//!   types; a value of the wrong type in a setting it doesn't type passes.
//! - The layout is checked first with the loader's checks, which upstream
//!   makes in `flattenV8`, and their errors are the loader's.

use serde::Deserialize;

use super::super::decode::decode;
use super::super::model_catalogs::CatalogSources;
use super::super::save::{
    Node as TreeNode, check_layout, delete_yaml_path, expand_config_aliases, flatten_v8,
    legacy_path, marshal, unmarshal, v8_aliases, yaml_path,
};
use super::super::v8::{V8_KEY_FAMILIES, V8_PATHS, V8_SHARED_STRUCT_PATHS};
use super::super::yaml::{
    Kind, Node, YamlError, duplicate_key_errors, is_merge, parse_document, push_type_error,
    resolve_node, scalar_string, type_error,
};
use super::super::yaml3::Kind as TreeKind;
use super::super::{ConfigError, ConfigErrorKind};
use super::schema::{CONFIG_LEGACY_CONFIG, Field, Type};

/// The v8 root sections that don't hold a moved legacy setting, with
/// open-ferry's `claude-cli`.
const EXTRA_ROOTS: [&str; 7] = [
    "models",
    "config-version",
    "api-keys",
    "plugins",
    "quota-exceeded",
    "client",
    "claude-cli",
];

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorKind::Invalid, message)
}

/// Checks that `data` is a valid v8 config file (upstream's
/// `ValidateV8Config`). The error is upstream's message.
pub fn validate_v8_config(data: &[u8]) -> Result<(), ConfigError> {
    let doc = unmarshal(data).map_err(|error| invalid(error.to_string()))?;
    let Some(root) = doc.content.first() else {
        return Err(invalid("empty config"));
    };
    check_layout(data).map_err(|error| invalid(error.to_string()))?;
    let mut flat = flatten_v8(root).map_err(|error| invalid(error.to_string()))?;
    let root = expand_config_aliases(root).map_err(|error| invalid(error.to_string()))?;
    let paths = V8_PATHS
        .iter()
        .chain(v8_aliases())
        .chain(V8_SHARED_STRUCT_PATHS);
    for &(old, current) in paths {
        if legacy_path(&root, old).is_some() {
            return Err(invalid(format!(
                "legacy field {old} is not accepted by v8; use {current}"
            )));
        }
    }
    for key in root.content.iter().step_by(2) {
        if !allowed_root(&key.value) {
            return Err(invalid(format!(
                "unknown v8 configuration section {}",
                key.value
            )));
        }
    }
    if let Some(groups) = yaml_path(&root, "api-keys")
        && groups.kind == TreeKind::Mapping
    {
        for key in groups.content.iter().step_by(2) {
            if !V8_KEY_FAMILIES
                .iter()
                .any(|&(_, current)| current == key.value)
            {
                return Err(invalid(format!("unknown API-key provider {}", key.value)));
            }
        }
    }
    delete_yaml_path(&mut flat, "config-version");
    // Empty struct containers are valid replacements. Only the known
    // structural paths go; empty user maps (headers, aliases, plugin
    // options) carry real values.
    for &(_, current) in V8_PATHS {
        let parts: Vec<&str> = current.split('.').collect();
        for end in (1..parts.len()).rev() {
            let container = parts.get(..end).unwrap_or_default().join(".");
            if yaml_path(&flat, &container).is_some_and(is_empty_mapping) {
                delete_yaml_path(&mut flat, &container);
            }
        }
    }
    let encoded = marshal(&flat).map_err(|error| invalid(error.to_string()))?;
    let decoded = strict_decode(&encoded).map_err(|error| invalid(error.message()))?;
    let models: ModelsOnly = decode(&decoded).map_err(|error| invalid(error.message()))?;
    models
        .models
        .validate()
        .map_err(|error| invalid(error.to_string()))
}

/// The `models` section of upstream's `legacyConfig`, which
/// `ValidateV8Config` checks once the strict decode passes.
#[derive(Default, Deserialize)]
#[serde(default, rename = "config.legacyConfig")]
struct ModelsOnly {
    models: CatalogSources,
}

/// `v8AllowedRoots`: the sections a v8 file may have.
fn allowed_root(key: &str) -> bool {
    EXTRA_ROOTS.contains(&key)
        || V8_PATHS.iter().any(|&(_, current)| {
            current
                .split_once('.')
                .map_or(current, |(section, _)| section)
                == key
        })
}

fn is_empty_mapping(node: &TreeNode) -> bool {
    node.kind == TreeKind::Mapping && node.content.is_empty()
}

/// Decodes `encoded` into upstream's `legacyConfig` with `KnownFields`,
/// checking its keys, and gives the document's root.
fn strict_decode(encoded: &[u8]) -> Result<Node, YamlError> {
    let text = std::str::from_utf8(encoded)
        .map_err(|_| YamlError::Syntax("yaml: input is not valid UTF-8".to_owned()))?;
    // A decoder at the end of its input returns io.EOF.
    let root = parse_document(text)?.ok_or_else(|| YamlError::Syntax("EOF".to_owned()))?;
    let mut strict = Strict { errors: Vec::new() };
    strict.structure(&root, &CONFIG_LEGACY_CONFIG)?;
    if strict.errors.is_empty() {
        Ok(root)
    } else {
        Err(YamlError::Type(strict.errors))
    }
}

/// yaml.v3's decode with unique keys and known fields, by the key checks
/// it makes. A fatal error stops it; type errors collect.
struct Strict {
    errors: Vec<String>,
}

impl Strict {
    /// Records `node`'s repeated keys; whether there were none. yaml.v3
    /// checks them before it decodes a mapping into anything.
    fn unique(&mut self, node: &Node) -> bool {
        let duplicates = duplicate_key_errors(node);
        let unique = duplicates.is_empty();
        for error in duplicates {
            push_type_error(&mut self.errors, error);
        }
        unique
    }

    /// Decoding `node` into the struct `ty`.
    fn structure(&mut self, node: &Node, ty: &Type) -> Result<(), YamlError> {
        match node.kind {
            Kind::Poison => Err(YamlError::Fatal(node.value.to_string())),
            Kind::Scalar => resolve_node(node).map(|_| ()),
            // A type error, which isn't checked; yaml.v3 doesn't look inside.
            Kind::Sequence => Ok(()),
            Kind::Mapping => self.fields(node, ty),
        }
    }

    /// `mappingStruct` with `KnownFields`.
    fn fields(&mut self, node: &Node, ty: &Type) -> Result<(), YamlError> {
        if !self.unique(node) {
            return Ok(());
        }
        let mut done = vec![false; ty.fields.len()];
        for (key, value) in node.pairs() {
            // The tree was written from one with its merge keys expanded.
            if is_merge(key) {
                continue;
            }
            let Some(name) = self.key_name(key)? else {
                continue;
            };
            let Some(index) = ty.fields.iter().position(|(field, _)| *field == name) else {
                push_type_error(
                    &mut self.errors,
                    format!(
                        "line {}: field {name} not found in type {}",
                        key.line, ty.name
                    ),
                );
                continue;
            };
            let (Some(done), Some(&(_, field))) = (done.get_mut(index), ty.fields.get(index))
            else {
                continue;
            };
            if *done {
                push_type_error(
                    &mut self.errors,
                    format!(
                        "line {}: field {name} already set in type {}",
                        key.line, ty.name
                    ),
                );
                continue;
            }
            *done = true;
            self.field(value, field)?;
        }
        Ok(())
    }

    /// A key decoded into a Go string; `None` when yaml.v3 skips it.
    fn key_name(&mut self, key: &Node) -> Result<Option<String>, YamlError> {
        match key.kind {
            Kind::Poison => Err(YamlError::Fatal(key.value.to_string())),
            Kind::Scalar => scalar_string(key),
            Kind::Mapping => {
                if self.unique(key) {
                    push_type_error(&mut self.errors, type_error(key, "string"));
                }
                Ok(None)
            }
            Kind::Sequence => {
                push_type_error(&mut self.errors, type_error(key, "string"));
                Ok(None)
            }
        }
    }

    /// Decoding `node` into a field.
    fn field(&mut self, node: &Node, field: Field) -> Result<(), YamlError> {
        match (field, node.kind) {
            (_, Kind::Poison) => Err(YamlError::Fatal(node.value.to_string())),
            (Field::Leaf, _) => self.leaf(node),
            (Field::Struct(ty), _) => self.structure(node, ty),
            (_, Kind::Scalar) => resolve_node(node).map(|_| ()),
            (Field::List(ty), Kind::Sequence) => node
                .content
                .iter()
                .try_for_each(|item| self.structure(item, ty)),
            (Field::MapList(ty), Kind::Mapping) => {
                if !self.unique(node) {
                    return Ok(());
                }
                for (key, list) in node.pairs() {
                    if is_merge(key) || self.key_name(key)?.is_none() {
                        continue;
                    }
                    self.field(list, Field::List(ty))?;
                }
                Ok(())
            }
            // A list given a mapping is a type error, after the key check.
            (Field::List(_), Kind::Mapping) => {
                self.unique(node);
                Ok(())
            }
            // A type error, which isn't checked.
            (Field::MapList(_), Kind::Sequence) => Ok(()),
        }
    }

    /// Decoding `node` into a value whose keys aren't fields: each mapping
    /// in it has its keys checked.
    fn leaf(&mut self, node: &Node) -> Result<(), YamlError> {
        match node.kind {
            Kind::Poison => Err(YamlError::Fatal(node.value.to_string())),
            Kind::Scalar => resolve_node(node).map(|_| ()),
            Kind::Sequence => node.content.iter().try_for_each(|item| self.leaf(item)),
            Kind::Mapping => {
                if !self.unique(node) {
                    return Ok(());
                }
                for (key, value) in node.pairs() {
                    if is_merge(key) {
                        continue;
                    }
                    self.leaf(key)?;
                    self.leaf(value)?;
                }
                Ok(())
            }
        }
    }
}
