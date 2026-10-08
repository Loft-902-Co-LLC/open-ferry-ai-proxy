// Ported from CLIProxyAPI internal/registry/codex_client_models.go
// (ValidateCodexClientModelsJSON and the embedded catalog) and
// internal/client/codex/models/models.go
// (loadCodexClientModelTemplatesSnapshot) (v8.0.20, MIT).
// models/codex_client_models.json is upstream's
// internal/registry/models/codex_client_models.json, unchanged.
// https://github.com/router-for-me/CLIProxyAPI

//! The Codex client model catalog: upstream's `codex_client_models.json`.
//! Each entry describes a model the way Codex clients expect it in their
//! model list, and serves as the template for that model's entry (see
//! [`crate::codex_models`]). The `gpt-5.5` entry is also the template for
//! models the catalog doesn't list.
//!
//! The catalog in use is [`CodexClientCatalog::current`]: the built-in one,
//! or the last valid one read from the file `models.codex-catalog` names
//! (see [`super::catalog_sources`]), published to [`super::CatalogStore`].
//!
//! The catalog is checked as upstream checks it before use: it needs a
//! default template, unique slugs, and the fields Codex can't do without.
//!
//! Deviations from upstream:
//! - A catalog is parsed once, when it is published, so it has no
//!   revisions, which upstream's readers use to know when to parse it again.
//!   `cmd/fetch_codex_models`, which downloads it while posing as the Codex
//!   CLI, isn't ported.
//! - Decode errors are worded differently from Go's.
//! - If the built-in catalog doesn't load, there are no templates and the
//!   model list is `null`, as upstream's is then; upstream also logs a
//!   warning.

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, LazyLock};

use open_ferry_translate::go;
use serde_json::{Map, Value};

use super::equal_fold;

/// The built-in catalog.
pub(crate) const EMBEDDED_CATALOG: &[u8] = include_bytes!("../../models/codex_client_models.json");

/// The slug of the template for models the catalog doesn't list.
pub const DEFAULT_TEMPLATE: &str = "gpt-5.5";

/// Why a Codex client model catalog was rejected. Its text is upstream's,
/// except for decode errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexCatalogError(String);

impl fmt::Display for CodexCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CodexCatalogError {}

impl CodexCatalogError {
    /// The error for a catalog read from `source`, worded as upstream's
    /// `loadCodexClientModelsFromBytes` words it.
    pub(crate) fn with_source(self, source: &str) -> Self {
        Self(format!("{source}: {}", self.0))
    }
}

/// The Codex client model catalog's entries, by slug.
#[derive(Clone, Debug)]
pub struct CodexClientCatalog {
    templates: HashMap<String, Map<String, Value>>,
    default_template: Map<String, Value>,
}

/// The built-in catalog, or `None` if it doesn't load.
static EMBEDDED: LazyLock<Option<Arc<CodexClientCatalog>>> = LazyLock::new(|| {
    CodexClientCatalog::from_json(EMBEDDED_CATALOG)
        .ok()
        .map(Arc::new)
});

impl CodexClientCatalog {
    /// The built-in catalog, or `None` if it doesn't load.
    pub fn embedded() -> Option<&'static Self> {
        EMBEDDED.as_deref()
    }

    /// The built-in catalog, shared, or `None` if it doesn't load.
    pub(crate) fn embedded_shared() -> Option<Arc<Self>> {
        EMBEDDED.clone()
    }

    /// The catalog in use: the built-in one, or the last valid one
    /// published; `None` if the built-in one doesn't load and none was
    /// published.
    pub fn current() -> Option<Arc<Self>> {
        super::CatalogStore::global().codex()
    }

    /// Reads a catalog in the format of upstream's
    /// `codex_client_models.json`, checked as
    /// [`validate_codex_client_models_json`] checks it.
    pub fn from_json(data: &[u8]) -> Result<Self, CodexCatalogError> {
        validate_codex_client_models_json(data)?;
        let mut templates = HashMap::new();
        let mut default_template = None;
        for model in decode(data)?.into_iter().flatten() {
            let slug = string_value(&model, "slug");
            if slug.is_empty() {
                continue;
            }
            if slug == DEFAULT_TEMPLATE {
                default_template = Some(model.clone());
            }
            templates.insert(slug.to_owned(), model);
        }
        let default_template = default_template.ok_or_else(missing_default)?;
        Ok(Self {
            templates,
            default_template,
        })
    }

    /// The entry whose slug, trimmed, is `slug`.
    pub fn template(&self, slug: &str) -> Option<&Map<String, Value>> {
        self.templates.get(slug)
    }

    /// The template for models the catalog doesn't list (`gpt-5.5`).
    pub fn default_template(&self) -> &Map<String, Value> {
        &self.default_template
    }

    /// Every entry, in no particular order.
    pub fn templates(&self) -> impl Iterator<Item = &Map<String, Value>> {
        self.templates.values()
    }
}

/// Checks that `data` is a Codex client model catalog upstream would serve
/// (upstream's `ValidateCodexClientModelsJSON`): at least one model, each
/// with a unique slug, the text, size and reasoning fields Codex needs, and
/// a `gpt-5.5` entry.
pub fn validate_codex_client_models_json(data: &[u8]) -> Result<(), CodexCatalogError> {
    let models = decode(data)?;
    if models.is_empty() {
        return Err(CodexCatalogError(
            "Codex client model catalog has no models".to_owned(),
        ));
    }
    let empty = Map::new();
    let mut seen = HashSet::with_capacity(models.len());
    for (index, model) in models.iter().enumerate() {
        let model = model.as_ref().unwrap_or(&empty);
        let slug = required_string(model, "slug").map_err(|err| {
            CodexCatalogError(format!("Codex client model catalog models[{index}]: {err}"))
        })?;
        if !seen.insert(slug) {
            return Err(CodexCatalogError(format!(
                "Codex client model catalog contains duplicate slug {}",
                go::quote(slug)
            )));
        }
        validate_model(model).map_err(|err| {
            CodexCatalogError(format!(
                "Codex client model catalog model {}: {err}",
                go::quote(slug)
            ))
        })?;
    }
    if !seen.contains(DEFAULT_TEMPLATE) {
        return Err(missing_default());
    }
    Ok(())
}

fn missing_default() -> CodexCatalogError {
    CodexCatalogError(format!(
        "Codex client model catalog is missing default template {}",
        go::quote(DEFAULT_TEMPLATE)
    ))
}

/// The catalog's models, decoded as Go decodes them into
/// `struct{ Models []map[string]any }`: the `models` key matches in any
/// case, the last one winning; a `null` list is empty and a `null` model is
/// `None`; anything else of the wrong type, or a number beyond a float64,
/// fails the decode.
fn decode(data: &[u8]) -> Result<Vec<Option<Map<String, Value>>>, CodexCatalogError> {
    let decode_error =
        |detail: String| CodexCatalogError(format!("decode Codex client model catalog: {detail}"));
    // Go's decoder reads each byte that isn't part of a UTF-8 character as
    // U+FFFD inside a string, and rejects it elsewhere, as this does.
    let root: Value = serde_json::from_str(&crate::multipart::lossy(data))
        .map_err(|err| decode_error(err.to_string()))?;
    check_numbers(&root).map_err(decode_error)?;
    let fields = match &root {
        Value::Null => return Ok(Vec::new()),
        Value::Object(fields) => fields,
        _ => return Err(decode_error("the catalog is not an object".to_owned())),
    };
    let Some(list) = fields
        .iter()
        .filter(|(key, _)| equal_fold(key, "models"))
        .map(|(_, value)| value)
        .next_back()
    else {
        return Ok(Vec::new());
    };
    let items = match list {
        Value::Null => return Ok(Vec::new()),
        Value::Array(items) => items,
        _ => return Err(decode_error("models is not an array".to_owned())),
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            Value::Null => Ok(None),
            Value::Object(model) => Ok(Some(model.clone())),
            _ => Err(decode_error(format!("models[{index}] is not an object"))),
        })
        .collect()
}

/// Fails on a number Go can't hold in a float64.
fn check_numbers(value: &Value) -> Result<(), String> {
    match value {
        Value::Number(number) => match number.as_f64() {
            Some(float) if float.is_finite() => Ok(()),
            _ => Err(format!("number {number} is out of range")),
        },
        Value::Array(items) => items.iter().try_for_each(check_numbers),
        Value::Object(fields) => fields.values().try_for_each(check_numbers),
        _ => Ok(()),
    }
}

/// Upstream's `validateCodexClientModel`.
fn validate_model(model: &Map<String, Value>) -> Result<(), String> {
    for field in [
        "display_name",
        "description",
        "base_instructions",
        "minimal_client_version",
        "visibility",
        "default_reasoning_level",
    ] {
        required_string(model, field)?;
    }

    let context_window = required_integer(model, "context_window", true)?;
    let max_context_window = required_integer(model, "max_context_window", true)?;
    if context_window > max_context_window {
        return Err(format!(
            "context_window {context_window} exceeds max_context_window {max_context_window}"
        ));
    }
    required_integer(model, "priority", false)?;

    let field = go::quote("supported_reasoning_levels");
    let levels = match model.get("supported_reasoning_levels") {
        Some(Value::Array(levels)) if !levels.is_empty() => levels,
        _ => return Err(format!("field {field} must be a non-empty array")),
    };
    let mut seen = HashSet::with_capacity(levels.len());
    for (index, level) in levels.iter().enumerate() {
        let Value::Object(level) = level else {
            return Err(format!("field {field} entry {index} must be an object"));
        };
        let effort = required_string(level, "effort")
            .map_err(|err| format!("field {field} entry {index}: {err}"))?;
        if !seen.insert(effort) {
            return Err(format!(
                "field {field} contains duplicate effort {}",
                go::quote(effort)
            ));
        }
    }
    let default_level = string_value(model, "default_reasoning_level");
    if !seen.contains(default_level) {
        return Err(format!(
            "default_reasoning_level {} is not listed in supported_reasoning_levels",
            go::quote(default_level)
        ));
    }
    Ok(())
}

/// `field`'s string, trimmed, which must not be empty
/// (`requiredCodexClientModelString`).
fn required_string<'a>(model: &'a Map<String, Value>, field: &str) -> Result<&'a str, String> {
    match string_value(model, field) {
        "" => Err(format!(
            "field {} must be a non-empty string",
            go::quote(field)
        )),
        value => Ok(value),
    }
}

/// `field`'s whole number, which must be positive, or with `positive` unset
/// not negative (`requiredCodexClientModelInteger`).
fn required_integer(
    model: &Map<String, Value>,
    field: &str,
    positive: bool,
) -> Result<i64, String> {
    let quoted = go::quote(field);
    let value = match model.get(field).and_then(Value::as_f64) {
        Some(value) if value.is_finite() && value.trunc() == value && value <= i64::MAX as f64 => {
            value
        }
        _ => return Err(format!("field {quoted} must be an integer")),
    };
    if positive && value <= 0.0 {
        return Err(format!("field {quoted} must be positive"));
    }
    if !positive && value < 0.0 {
        return Err(format!("field {quoted} must not be negative"));
    }
    // Whole numbers within range convert exactly, as in Go.
    Ok(value as i64)
}

/// `key`'s string, trimmed, or empty when it isn't a string.
fn string_value<'a>(model: &'a Map<String, Value>, key: &str) -> &'a str {
    model.get(key).and_then(Value::as_str).unwrap_or("").trim()
}
