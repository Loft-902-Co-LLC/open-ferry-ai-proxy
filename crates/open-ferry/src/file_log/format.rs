// Ported from CLIProxyAPI internal/logging/global_logger.go (LogFormatter,
// logFieldOrder, quotedLogFields, pluginPathFieldOrder, formatLogFieldValue)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The log lines, as upstream's `LogFormatter` writes them:
//!
//! ```text
//! [2006-01-02 15:04:05] [5678abcd] [info ] [manager.rs:524] message provider=codex
//! ```
//!
//! The local time, the request's short ID (`--------` when the line isn't
//! a request's), the level padded to five (`warn` for a warning), where
//! the line was logged, the message, and then the fields upstream shows, in
//! its order. The request ID is the `request_id` field, of the event or of
//! a span it is in, such as the access log's request span.
//!
//! Every email address in a line is masked, whoever logged it
//! ([`mask_emails`]): `main.log`, standard output and the TUI's logs tab
//! all get the line formatted here.
//!
//! Deviations from upstream:
//! - Email addresses are masked, the message's and the fields' alike, in
//!   file names and paths too: `claude-john@example.com.json` is written
//!   `claude-j***@e***.com.json`. Upstream logs an account's email, and
//!   the auth file names that hold one, as they are.
//! - A field is quoted, where upstream quotes it, when its value was
//!   recorded as text: a string, or a value logged with `%` or `?`. Go
//!   quotes a `string`. Numbers and booleans aren't quoted.
//! - Fields come from the event and from the spans it is in, the event's
//!   winning, where upstream's lines have their own only.
//! - A line that doesn't say which file logged it shows its target in
//!   place of the file and line. Upstream always knows the caller.

use std::borrow::Cow;
use std::fmt::{self, Write as _};
use std::sync::Arc;

use chrono::{DateTime, Local};
use open_ferry_core::observe::mask::mask_emails;
use open_ferry_core::observe::short_request_id;
use open_ferry_translate::go::{format_float_g, quote};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber, span};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use super::writer::Output;

/// The field that holds the request's ID.
pub(super) const REQUEST_ID: &str = "request_id";

/// The request ID column of a line that isn't a request's.
const NO_REQUEST_ID: &str = "--------";

/// The fields a line shows, in this order (upstream's `logFieldOrder`).
const FIELD_ORDER: [&str; 34] = [
    "provider",
    "model",
    "plugin_id",
    "plugin_name",
    "source_id",
    "version",
    "active_version",
    "retired_version",
    "overwritten",
    "mode",
    "budget",
    "level",
    "original_mode",
    "original_value",
    "min",
    "max",
    "clamped_to",
    "error",
    "credential",
    "auth_id",
    "auth_index",
    "connection",
    "proxy_scheme",
    "remote_transport",
    "operation",
    "upstream_host",
    "reused",
    "was_idle",
    "idle_time",
    "media_session_id",
    "call_id",
    "peer",
    "state",
    "reason",
];

/// The fields whose text is quoted (upstream's `quotedLogFields`).
const QUOTED: [&str; 13] = [
    "credential",
    "auth_id",
    "auth_index",
    "upstream_host",
    "operation",
    "connection",
    "proxy_scheme",
    "remote_transport",
    "media_session_id",
    "call_id",
    "peer",
    "state",
    "reason",
];

/// The path fields a line shows after the others, only for a plugin's
/// lines (upstream's `pluginPathFieldOrder`).
const PLUGIN_PATHS: [&str; 3] = ["path", "active_path", "retired_path"];

/// A field's value, as it was recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum FieldValue {
    /// Text: a string, or a value logged with `%` or `?`.
    Text(String),
    /// A number or a boolean.
    Plain(String),
}

impl FieldValue {
    fn as_str(&self) -> &str {
        match self {
            Self::Text(text) | Self::Plain(text) => text,
        }
    }
}

/// The fields of an event, and of the spans it is in, that a line can
/// show.
#[derive(Clone, Debug, Default)]
pub(super) struct Fields(Vec<(&'static str, FieldValue)>);

impl Fields {
    /// Sets the field `name`.
    pub(super) fn set(&mut self, name: &'static str, value: FieldValue) {
        match self.0.iter_mut().find(|(field, _)| *field == name) {
            Some((_, slot)) => *slot = value,
            None => self.0.push((name, value)),
        }
    }

    /// Sets the field `name` to the text `value`.
    #[cfg(test)]
    pub(super) fn text(mut self, name: &'static str, value: &str) -> Self {
        self.set(name, FieldValue::Text(value.to_owned()));
        self
    }

    fn get(&self, name: &str) -> Option<&FieldValue> {
        self.0
            .iter()
            .find(|(field, _)| *field == name)
            .map(|(_, value)| value)
    }

    fn extend(&mut self, other: &Self) {
        for (name, value) in &other.0 {
            self.set(name, value.clone());
        }
    }
}

/// What a line is made of (the parts of upstream's logrus entry the
/// formatter reads).
pub(super) struct Entry<'a> {
    /// When it was logged.
    pub(super) time: DateTime<Local>,
    /// Its level.
    pub(super) level: Level,
    /// Where it was logged, as `file:line`, if known.
    pub(super) caller: Option<String>,
    /// The message.
    pub(super) message: &'a str,
    /// The fields.
    pub(super) fields: &'a Fields,
}

/// `entry` as a line, with its newline (upstream's `LogFormatter.Format`).
pub(super) fn format(entry: &Entry<'_>) -> String {
    let mut line = String::with_capacity(64 + entry.message.len());
    let request_id = match entry.fields.get(REQUEST_ID) {
        Some(FieldValue::Text(id)) if !id.is_empty() => short_request_id(id),
        _ => NO_REQUEST_ID,
    };
    let _ = write!(
        line,
        "[{}] [{request_id}] [{:<5}] ",
        entry.time.format("%Y-%m-%d %H:%M:%S"),
        level_name(entry.level),
    );
    if let Some(caller) = &entry.caller {
        let _ = write!(line, "[{caller}] ");
    }
    line.push_str(entry.message.trim_end_matches(['\r', '\n']));
    for name in FIELD_ORDER {
        if let Some(value) = entry.fields.get(name) {
            let _ = write!(line, " {name}={}", field_text(name, value));
        }
    }
    let plugin = entry.fields.get("plugin_id");
    if plugin.is_some_and(|id| !id.as_str().trim().is_empty()) {
        for name in PLUGIN_PATHS {
            if let Some(value) = entry.fields.get(name) {
                let _ = write!(line, " {name}={}", value.as_str());
            }
        }
    }
    line.push('\n');
    match mask_emails(&line) {
        Cow::Borrowed(_) => line,
        Cow::Owned(masked) => masked,
    }
}

/// How the field `name` shows `value` (upstream's `formatLogFieldValue`).
fn field_text(name: &str, value: &FieldValue) -> String {
    match value {
        FieldValue::Text(text) if QUOTED.contains(&name) => quote(text),
        value => value.as_str().to_owned(),
    }
}

/// The level as logrus names it, `warning` shortened to `warn`.
fn level_name(level: Level) -> &'static str {
    match level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    }
}

/// Whether a line can show the field `name`.
fn shown(name: &str) -> bool {
    name == REQUEST_ID || FIELD_ORDER.contains(&name) || PLUGIN_PATHS.contains(&name)
}

/// Records the fields a line can show, and the message.
struct Visitor<'a> {
    fields: &'a mut Fields,
    message: Option<&'a mut String>,
}

impl Visitor<'_> {
    fn value(&mut self, field: &Field, value: FieldValue) {
        let name = field.name();
        if name == "message" {
            if let Some(message) = self.message.as_deref_mut() {
                *message = value.as_str().to_owned();
            }
        } else if shown(name) {
            self.fields.set(name, value);
        }
    }
}

impl Visit for Visitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.value(field, FieldValue::Text(value.to_owned()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.value(field, FieldValue::Text(format!("{value:?}")));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.value(field, FieldValue::Text(value.to_string()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.value(field, FieldValue::Plain(value.to_string()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.value(field, FieldValue::Plain(value.to_string()));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.value(field, FieldValue::Plain(value.to_string()));
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.value(field, FieldValue::Plain(value.to_string()));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.value(field, FieldValue::Plain(value.to_string()));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.value(field, FieldValue::Plain(format_float_g(value)));
    }
}

/// The layer that formats each event as a line and writes it to the
/// output.
pub(super) struct FormatLayer {
    output: Arc<Output>,
}

impl FormatLayer {
    pub(super) fn new(output: Arc<Output>) -> Self {
        Self { output }
    }
}

impl<S> Layer<S> for FormatLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut Visitor {
            fields: &mut fields,
            message: None,
        });
        if let Some(span) = ctx.span(id)
            && !fields.0.is_empty()
        {
            span.extensions_mut().insert(fields);
        }
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut extensions = span.extensions_mut();
        if let Some(fields) = extensions.get_mut::<Fields>() {
            values.record(&mut Visitor {
                fields,
                message: None,
            });
            return;
        }
        let mut fields = Fields::default();
        values.record(&mut Visitor {
            fields: &mut fields,
            message: None,
        });
        if !fields.0.is_empty() {
            extensions.insert(fields);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(span_fields) = span.extensions().get::<Fields>() {
                    fields.extend(span_fields);
                }
            }
        }
        let mut message = String::new();
        event.record(&mut Visitor {
            fields: &mut fields,
            message: Some(&mut message),
        });
        let metadata = event.metadata();
        let caller = match (metadata.file(), metadata.line()) {
            (Some(file), Some(line)) => format!("{}:{line}", base_name(file)),
            _ => metadata.target().to_owned(),
        };
        let line = format(&Entry {
            time: Local::now(),
            level: *metadata.level(),
            caller: Some(caller),
            message: &message,
            fields: &fields,
        });
        self.output.write(line.into_bytes());
    }
}

/// `path`'s last element, whichever separator it uses (Go's
/// `filepath.Base` on Windows).
fn base_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}
