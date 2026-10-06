// Ported from CLIProxyAPI internal/runtime/executor/helps/payload_media.go
// (ApplyMediaPayloadConfig, outOrBody, mediaPayloadJSON) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The payload rules applied to the body of an image or video call, JSON
//! or a `multipart/form-data` form (upstream's `ApplyMediaPayloadConfig`).
//!
//! The rules see a form as a JSON object, its view: each field a string,
//! each file an object of its `filename`, its `content_type` and its
//! `data` in standard base64, and a name sent more than once an array of
//! its values in the order sent. A view the rules change is written back
//! as a new form with a new random boundary: an array becomes a part for
//! each item, an object with a `filename` a file, and anything else a field
//! holding gjson's `String()` of it. A view they leave alone sends the form
//! as it came, byte for byte, with its own content type. A JSON body is
//! treated as any other body.
//!
//! Rules match as they do for any body (see the module above), with no
//! root. Their defaults check the client's request as it came, in its own
//! view, not translated. Nothing here logs: a view holds the prompt and the
//! images.
//!
//! Deviations from upstream:
//! - A form written back has its fields in the view's order (name order,
//!   then any a rule added, in the order added); upstream's map gives them
//!   in random order.
//! - A body that is neither JSON nor a form has no rules applied and goes
//!   as it came, where upstream runs sjson on its bytes; a client's request
//!   that is neither reads as having no fields.
//! - Whether the rules changed a view is judged on its value; upstream
//!   compares the text sjson wrote.
//! - A changed JSON body is written compactly, as every executor here
//!   writes a body the rules changed.

use std::collections::BTreeMap;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose;
use bytes::Bytes;
use open_ferry_core::config::Config;
use open_ferry_core::exec::{Format, Options, Request};
use open_ferry_core::multipart::{
    self, Header, Reader, Writer, format_media_type, parse_media_type,
};
use open_ferry_translate::go::base64::{CorruptInputError, STD};
use open_ferry_translate::go::format_float;
use open_ferry_translate::json::exact;
use serde_json::{Map, Value};

use super::{Call, Rules, apply_call, installed, requested_model, select};

/// Where an image or video body goes.
#[derive(Clone, Copy, Debug)]
pub struct MediaTarget<'a> {
    /// The executor's identifier (upstream's `executor`).
    pub executor: &'a str,
    /// The model sent upstream.
    pub model: &'a str,
    /// The format the body is in (upstream's `protocol`, `openai` for
    /// every caller).
    pub protocol: &'a Format,
}

/// Why the rules couldn't be applied to an image or video body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaError {
    /// The body, or the client's request, has a form content type but
    /// doesn't read as one.
    Form(multipart::Error),
    /// A file's `data`, after the rules, isn't standard base64.
    Data {
        /// The file's field name.
        field: String,
        /// Where the base64 broke.
        error: CorruptInputError,
    },
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaError::Form(error) => write!(f, "{error}"),
            MediaError::Data { field, error } => write!(f, "multipart payload {field}: {error}"),
        }
    }
}

impl std::error::Error for MediaError {}

impl From<multipart::Error> for MediaError {
    fn from(error: multipart::Error) -> Self {
        MediaError::Form(error)
    }
}

/// What a body is to the rules (upstream's `mediaPayloadJSON`).
pub(super) enum View {
    /// A JSON body.
    Json(Value),
    /// A form's view.
    Form(Value),
    /// Neither, empty or not.
    Other { empty: bool },
}

/// Applies the payload rules to `body`, an image or video body of
/// `content_type` going to `target` for `request`, and returns the body to
/// send and its content type (upstream's `ApplyMediaPayloadConfig`). With
/// no `config`, only a Codex client's tool parameters can change.
pub fn apply_media(
    config: Option<&Config>,
    target: &MediaTarget<'_>,
    request: &Request,
    options: &Options,
    body: Bytes,
    content_type: &str,
) -> Result<(Bytes, String), MediaError> {
    let rules = select(config, installed);
    apply_media_rules(rules.get(), target, request, options, body, content_type)
}

/// [`apply_media`] with the rules given, `None` for no config.
pub(super) fn apply_media_rules(
    rules: Option<&Rules>,
    target: &MediaTarget<'_>,
    request: &Request,
    options: &Options,
    body: Bytes,
    content_type: &str,
) -> Result<(Bytes, String), MediaError> {
    let view = view(&body, content_type.as_bytes())?;
    let original = if options.original_request.is_empty() {
        &request.payload
    } else {
        &options.original_request
    };
    let original_type = options
        .headers
        .get(http::header::CONTENT_TYPE)
        .map(|value| value.as_bytes())
        .unwrap_or_default();
    // The client's request is usually the body itself, whose view is
    // already read.
    let original = if *original == body && original_type == content_type.as_bytes() {
        None
    } else {
        Some(self::view(original, original_type)?)
    };
    let (mut value, form) = match view {
        View::Json(value) => (value, false),
        View::Form(value) => (value, true),
        View::Other { .. } => return Ok((body, content_type.to_owned())),
    };
    let before = value.clone();
    let requested = requested_model(request, options);
    let call = Call {
        executor: target.executor,
        protocol: target.protocol.as_str(),
        from: options.source_format.as_str(),
        model: target.model,
        requested_model: &requested,
        request_path: options.metadata.request_path.trim(),
        root: "",
        headers: &options.headers,
        tracked: &[],
    };
    let source = || match original {
        None => Some(before.clone()),
        Some(View::Json(value) | View::Form(value)) => Some(value),
        Some(View::Other { empty: true }) => None,
        Some(View::Other { empty: false }) => Some(Value::Object(Map::new())),
    };
    apply_call(rules, &call, source, &mut value);
    if value == before {
        return Ok((body, content_type.to_owned()));
    }
    if !form {
        return Ok((Bytes::from(value.to_string()), content_type.to_owned()));
    }
    let mut writer = Writer::new();
    if let Value::Object(fields) = &value {
        for (key, value) in fields {
            let items = match value {
                Value::Array(items) => items.as_slice(),
                value => std::slice::from_ref(value),
            };
            for item in items {
                write_item(&mut writer, key, item)?;
            }
        }
    }
    let content_type = writer.form_data_content_type();
    Ok((Bytes::from(writer.finish()), content_type))
}

/// Writes one item of field `key` of a changed view: a file when it is an
/// object with a `filename`, else a field.
fn write_item(writer: &mut Writer, key: &str, item: &Value) -> Result<(), MediaError> {
    let file = match item {
        Value::Object(file) if file.contains_key("filename") => file,
        item => {
            writer.write_field(key, gjson_string(Some(item)).as_bytes());
            return Ok(());
        }
    };
    let data = STD
        .decode(gjson_string(file.get("data")))
        .map_err(|error| MediaError::Data {
            field: key.to_owned(),
            error,
        })?;
    let filename = gjson_string(file.get("filename"));
    let mut header = Header::new();
    header.set(
        "Content-Disposition",
        format_media_type(
            "form-data",
            &[("name", key.as_bytes()), ("filename", filename.as_bytes())],
        ),
    );
    header.set("Content-Type", gjson_string(file.get("content_type")));
    writer.write_part(&header, &data);
    Ok(())
}

/// What `body` of `content_type` is to the rules: JSON, a form's view, or
/// neither; an error when the content type is a form's but the body
/// doesn't read as one (upstream's `mediaPayloadJSON`).
pub(super) fn view(body: &Bytes, content_type: &[u8]) -> Result<View, MediaError> {
    if let Ok(value) = exact::from_slice(body) {
        return Ok(View::Json(value));
    }
    let boundary = match parse_media_type(content_type) {
        Ok((kind, params)) if kind == "multipart/form-data" => {
            params.get("boundary").cloned().unwrap_or_default()
        }
        _ => {
            return Ok(View::Other {
                empty: body.is_empty(),
            });
        }
    };
    let mut reader = Reader::new(body.clone(), &boundary);
    let mut fields: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    while let Some(part) = reader.next_part()? {
        let name = part.form_name();
        let file_name = part.file_name();
        let content_type = part.header.get_str("Content-Type");
        let data = part.read_all()?;
        let value = if file_name.is_empty() {
            Value::String(lossy(&data))
        } else {
            let mut file = Map::new();
            file.insert("content_type".to_owned(), Value::String(content_type));
            file.insert(
                "data".to_owned(),
                Value::String(general_purpose::STANDARD.encode(&data)),
            );
            file.insert("filename".to_owned(), Value::String(file_name));
            Value::Object(file)
        };
        fields.entry(name).or_default().push(value);
    }
    let mut view = Map::new();
    for (name, mut values) in fields {
        let value = if values.len() == 1 {
            values.pop().unwrap_or(Value::Null)
        } else {
            Value::Array(values)
        };
        view.insert(name, value);
    }
    Ok(View::Form(Value::Object(view)))
}

/// `bytes` as text, each byte that isn't part of a valid character read as
/// U+FFFD, as Go's JSON encoder writes a string.
fn lossy(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for _ in chunk.invalid() {
            out.push(char::REPLACEMENT_CHARACTER);
        }
    }
    out
}

/// gjson's `String()` of a value: a string as it is, `true` or `false`, a
/// whole number as written and any other number as Go's shortest decimal,
/// other JSON as compact text, and nothing or `null` as empty.
fn gjson_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Number(number)) => {
            let raw = number.to_string();
            let digits = raw.strip_prefix('-').unwrap_or(&raw);
            if digits.bytes().all(|b| b.is_ascii_digit()) {
                raw
            } else {
                format_float(number.as_f64().unwrap_or_default())
            }
        }
        Some(other) => other.to_string(),
    }
}
