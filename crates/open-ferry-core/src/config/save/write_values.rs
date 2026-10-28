//! Values supplied before encoding, compared with what the file will load.

use std::collections::BTreeMap;

use crate::config::decode::decode;
use crate::config::layout::{AnyValue, decode_any_value};
use crate::config::normalize::post_process;
use crate::config::save::{self, generate, merge, tree};
use crate::config::yaml;
use crate::config::yaml3::encode::{self, Value};
use crate::config::yaml3::{Kind, Node, STR_TAG, Style};
use crate::config::{Config, v8};

use super::{SaveError, SaveErrorKind};

pub(crate) struct IntendedValues {
    input: Option<Vec<u8>>,
    values: Option<Snapshot>,
    raw: Option<(Node, bool)>,
}

struct Snapshot {
    document: Node,
    generated: Option<Node>,
    migrate: bool,
}

impl IntendedValues {
    /// Direct writes and undo pass their actual input, with no intervening encoder.
    pub(crate) fn unchanged(input: &[u8]) -> Self {
        Self {
            input: Some(input.to_vec()),
            values: None,
            raw: None,
        }
    }

    pub(crate) fn settings(data: &[u8], cfg: &Config, migrate: bool) -> Result<Self, SaveError> {
        let doc = save::unmarshal(data)?;
        let migrate = migrate
            || doc
                .content
                .first()
                .map(save::is_v8_config_layout)
                .transpose()?
                .unwrap_or(false);
        Self::capture(doc, cfg, migrate, false)
    }

    pub(crate) fn bytes(data: &[u8]) -> Result<Self, SaveError> {
        let doc = save::unmarshal(data)?;
        let migrate = doc
            .content
            .first()
            .map(save::is_v8_config_layout)
            .transpose()?
            .unwrap_or(false);
        Ok(Self {
            input: Some(data.to_vec()),
            values: None,
            raw: Some((doc, migrate)),
        })
    }

    pub(crate) fn node(doc: &Node, migrate: bool) -> Result<Self, SaveError> {
        // Keep the pre-encoding tree now; decode it after the output's
        // existing load check, so invalid requests keep their error class.
        Ok(Self {
            input: None,
            values: None,
            raw: Some((doc.clone(), migrate)),
        })
    }

    fn capture(
        doc: Node,
        cfg: &Config,
        migrate: bool,
        allow_opaque: bool,
    ) -> Result<Self, SaveError> {
        let mut normalized = cfg.clone();
        post_process(&mut normalized).map_err(check_error)?;
        let generated = match generated_node(&normalized) {
            Ok(node) => Some(node),
            Err(_) if allow_opaque => None,
            Err(error) => return Err(error),
        };
        Ok(Self {
            input: None,
            values: Some(Snapshot {
                document: doc,
                generated,
                migrate,
            }),
            raw: None,
        })
    }

    pub(crate) fn difference(
        &self,
        data: &[u8],
        actual: &Config,
    ) -> Result<Option<String>, SaveError> {
        if let Some(input) = &self.input {
            if input == data {
                return Ok(None);
            }
            if self.raw.is_none() {
                return Ok(Some("$".to_owned()));
            }
        }
        if let Some((doc, migrate)) = &self.raw {
            // A canonical v8 mapping can discard a malformed legacy alias.
            // Decode the same normalized layout that this write will load.
            let cfg = if *migrate {
                config_from_node(&normalized_document(doc, true)?)?
            } else {
                config_from_node(doc)?
            };
            return Self::capture(doc.clone(), &cfg, *migrate, true)?.difference(data, actual);
        }
        let expected = self.values.as_ref().expect("a captured save intent");
        let actual_generated = if expected.generated.is_some() {
            match generated_node(actual) {
                Ok(node) => Some(node),
                Err(_) => return Ok(Some("$".into())),
            }
        } else {
            None
        };
        // Compare the safe merged intent: omitted defaults and unmodeled
        // source fields follow the same preservation rules as the writer.
        let wanted = canonical(
            &expected.document,
            expected.generated.as_ref(),
            expected.migrate,
        )?;
        let actual_doc = save::unmarshal(data)?;
        let got = canonical(&actual_doc, actual_generated.as_ref(), expected.migrate)?;
        Ok(wanted.difference(&got, "$"))
    }
}

fn check_error(error: impl std::fmt::Display) -> SaveError {
    SaveError::new(SaveErrorKind::Check, error.to_string())
}

fn config_from_node(doc: &Node) -> Result<Config, SaveError> {
    let Some(root) = doc
        .content
        .first()
        .filter(|_| doc.kind == Kind::Document)
        .or_else(|| (doc.kind != Kind::Document && doc.kind != Kind::Zero).then_some(doc))
    else {
        return Ok(Config::default());
    };
    let node = yaml::from_writer_node(root).map_err(|error| check_error(error.message()))?;
    let flat = v8::flatten_v8(&node).map_err(check_error)?;
    decode(&flat.root).map_err(|error| check_error(error.message()))
}

/// Quotes are used only for the comparison's private copy, never the saved file.
fn quoted_node(node: &mut Node) {
    node.head_comment.clear();
    node.line_comment.clear();
    node.foot_comment.clear();
    if node.kind == Kind::Scalar {
        node.style = Style::DOUBLE_QUOTED
            | if node.tag == STR_TAG {
                Style::NONE
            } else {
                Style::TAGGED
            };
    }
    for child in &mut node.content {
        quoted_node(child);
    }
}

fn quoted_value(value: &mut Value) {
    match value {
        Value::Str(text) => {
            let mut node = Node::scalar(STR_TAG, text);
            node.style = Style::DOUBLE_QUOTED;
            *value = Value::Node(node);
        }
        Value::Node(node) => quoted_node(node),
        Value::Seq(items) => items.iter_mut().for_each(quoted_value),
        Value::Map(entries) => {
            for (key, value) in entries {
                quoted_value(key);
                quoted_value(value);
            }
        }
        Value::Struct(fields) => {
            for field in fields {
                quoted_value(&mut field.value);
            }
        }
        _ => {}
    }
}

fn generated_node(cfg: &Config) -> Result<Node, SaveError> {
    let mut value = generate::legacy_config(cfg)
        .map_err(|error| SaveError::new(SaveErrorKind::Unwritable, error.0))?;
    quoted_value(&mut value);
    let bytes = encode::marshal(&value)?;
    let mut doc = save::unmarshal(&bytes)?;
    Ok(doc.content.pop().unwrap_or_else(Node::mapping))
}

fn normalized_document(doc: &Node, migrate: bool) -> Result<Node, SaveError> {
    let mut doc = doc.clone();
    if doc.kind == Kind::Document
        && let Some(root) = doc.content.first_mut()
    {
        *root = tree::expand_config_aliases(root)?;
        save::omit_stable_update_channel(root);
    }
    quoted_node(&mut doc);
    if migrate {
        let bytes = encode::faithful_node(&doc, 2)?;
        doc = save::normalize_config_layout_doc(&bytes, true)?.0;
    }
    Ok(doc)
}

fn canonical(doc: &Node, generated: Option<&Node>, migrate: bool) -> Result<Semantic, SaveError> {
    let doc = normalized_document(doc, migrate)?;
    let first = doc
        .content
        .first()
        .filter(|_| doc.kind == Kind::Document)
        .unwrap_or(&doc);
    let mut root = if first.kind == Kind::Zero {
        Node::mapping()
    } else {
        save::v8::flatten_v8(first)?
    };
    let Some(generated) = generated else {
        return semantic(&root);
    };
    if !migrate {
        merge::remove_legacy_auth_block(&mut root);
        merge::remove_removed_integration_keys(&mut root);
        merge::remove_legacy_generative_language_keys(&mut root);
    }
    merge::remove_legacy_openai_compat_api_keys(&mut root);
    for key in save::OAUTH_MAPS {
        merge::prune_mapping_to_generated_keys(&mut root, generated, key);
    }
    merge::prune_routing_quota(&mut root, generated);
    merge::prune_mapping_to_generated_keys(&mut root, generated, "self-update");
    merge::merge_mapping_preserve(&mut root, generated, &mut Vec::new());
    let mut cfg = config_from_node(&Node {
        kind: Kind::Document,
        content: vec![root.clone()],
        ..Node::default()
    })?;
    post_process(&mut cfg).map_err(check_error)?;
    let normalized = generated_node(&cfg)?;
    normalize_values(&mut root, &normalized, &mut Vec::new(), false);
    semantic(&root)
}

// These paths are preserved from the source by merge::UNTYPED. The typed
// config only supplies their defaults, so it cannot normalize their values.
const PRESERVED_SOURCE_PATHS: &[&[&str]] = &[
    &["claude-code"],
    &["gpt-image-2-base-model"],
    &["video-result-auth-cache-ttl"],
    &["credential-concurrency"],
    &["credential-in-flight"],
    &["plugins"],
    &["pprof"],
    &["discovery"],
    &["antigravity-signature-cache-enabled"],
    &["antigravity-signature-bypass-strict"],
    &["antigravity"],
    &["devin"],
    &["claude-header-defaults", "user-agent"],
    &["claude-header-defaults", "package-version"],
    &["claude-header-defaults", "runtime-version"],
    &["claude-header-defaults", "os"],
    &["claude-header-defaults", "arch"],
    &["claude-header-defaults", "timeout"],
    &["claude-header-defaults", "timezone"],
    &["claude-header-defaults", "stabilize-device-profile"],
    &["disable-claude-cloak-mode"],
    &["codex", "disable-codex-cloaking"],
    &["codex", "live-media-relay"],
    &["codex-header-defaults", "user-agent"],
    &["claude-api-key", "cloak"],
    &["claude-api-key", "fingerprint-profile"],
    &["claude-api-key", "experimental-cch-signing"],
    &["codex-api-key", "disable-codex-cloaking"],
    &["xai-api-key", "disable-codex-cloaking"],
    &["meta-api-key", "disable-codex-cloaking"],
];

fn preserved_source_path(path: &[String]) -> bool {
    PRESERVED_SOURCE_PATHS.iter().any(|prefix| {
        path.len() >= prefix.len() && prefix.iter().zip(path).all(|(a, b)| *a == b.as_str())
    })
}

/// Use the loader's values for typed fields, including null collections and
/// trimmed model aliases. Keep source fields that the config does not type.
fn normalize_values(dst: &mut Node, src: &Node, path: &mut Vec<String>, in_sequence: bool) {
    if preserved_source_path(path) {
        return;
    }
    match (dst.kind, src.kind) {
        (Kind::Mapping, Kind::Mapping) => {
            let mut index = 0;
            while index + 1 < dst.content.len() {
                let key = dst.content[index].value.clone();
                path.push(key.clone());
                if let Some(i) = tree::find_map_key_index(src, &key) {
                    normalize_values(
                        &mut dst.content[index + 1],
                        &src.content[i + 1],
                        path,
                        in_sequence,
                    );
                } else if (in_sequence
                    || (path.len() == 2 && save::OAUTH_MAPS.contains(&path[0].as_str())))
                    && !preserved_source_path(path)
                {
                    dst.content.drain(index..index + 2);
                    path.pop();
                    continue;
                }
                path.pop();
                index += 2;
            }
            for (key, value) in src.pairs() {
                if tree::find_map_key_index(dst, &key.value).is_none() {
                    dst.content.push(key.clone());
                    dst.content.push(value.clone());
                }
            }
        }
        (Kind::Sequence, Kind::Sequence) => {
            dst.content.truncate(src.content.len());
            for (i, value) in src.content.iter().enumerate() {
                if let Some(old) = dst.content.get_mut(i) {
                    normalize_values(old, value, path, true);
                } else {
                    dst.content.push(value.clone());
                }
            }
        }
        _ => *dst = src.clone(),
    }
}

enum Semantic {
    Scalar(AnyValue),
    Opaque(String, String),
    Seq(Vec<Semantic>),
    Map(BTreeMap<String, Semantic>),
    Pairs(Vec<(Semantic, Semantic)>),
}

fn semantic(node: &Node) -> Result<Semantic, SaveError> {
    let node = yaml::from_writer_node(node).map_err(|error| check_error(error.message()))?;
    fn visit(node: &yaml::Node) -> Semantic {
        match node.kind {
            yaml::Kind::Sequence => Semantic::Seq(node.content.iter().map(visit).collect()),
            yaml::Kind::Mapping
                if node
                    .content
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .all(|p| p[0].tag == STR_TAG) =>
            {
                Semantic::Map(
                    node.content
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|p| (p[0].value.to_string(), visit(&p[1])))
                        .collect(),
                )
            }
            yaml::Kind::Mapping => Semantic::Pairs(
                node.content
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| (visit(&p[0]), visit(&p[1])))
                    .collect(),
            ),
            _ => match decode_any_value(node) {
                Ok(value) => Semantic::Scalar(value),
                Err(_) => Semantic::Opaque(node.tag.to_string(), node.value.to_string()),
            },
        }
    }
    Ok(visit(&node))
}

impl Semantic {
    fn difference(&self, actual: &Self, path: &str) -> Option<String> {
        match (self, actual) {
            (Self::Scalar(AnyValue::Float(a)), Self::Scalar(AnyValue::Float(b)))
                if a.is_nan() && b.is_nan() =>
            {
                None
            }
            (Self::Scalar(a), Self::Scalar(b)) if a == b => None,
            (Self::Opaque(a, x), Self::Opaque(b, y)) if a == b && x == y => None,
            (Self::Seq(a), Self::Seq(b)) => {
                for (i, (a, b)) in a.iter().zip(b).enumerate() {
                    if let Some(p) = a.difference(b, &format!("{path}[{i}]")) {
                        return Some(p);
                    }
                }
                (a.len() != b.len()).then(|| format!("{path}[{}]", a.len().min(b.len())))
            }
            (Self::Map(a), Self::Map(b)) => {
                for (key, a) in a {
                    let p = format!("{path}.{key}");
                    let Some(b) = b.get(key) else {
                        return Some(p);
                    };
                    if let Some(p) = a.difference(b, &p) {
                        return Some(p);
                    }
                }
                b.keys()
                    .find(|k| !a.contains_key(*k))
                    .map(|k| format!("{path}.{k}"))
            }
            (Self::Pairs(a), Self::Pairs(b)) => {
                if a.len() != b.len() {
                    return Some(path.to_owned());
                }
                for (key, value) in a {
                    let Some((_, other)) = b
                        .iter()
                        .find(|(other, _)| key.difference(other, "$").is_none())
                    else {
                        return Some(path.to_owned());
                    };
                    if let Some(p) = value.difference(other, &format!("{path}[map-key]")) {
                        return Some(p);
                    }
                }
                None
            }
            _ => Some(path.to_owned()),
        }
    }
}
