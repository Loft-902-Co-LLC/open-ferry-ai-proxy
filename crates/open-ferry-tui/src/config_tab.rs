// Ported from CLIProxyAPI internal/tui/config_tab.go (newConfigTabModel, Init,
// fetchConfig, Update, handleNormalKey, handleEditingKey, toggleBool,
// submitEdit, configFieldEditValue, SetSize, ensureCursorVisible, View,
// renderContent, parseConfig, fieldSection, getBoolNested) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The config tab: the settings the management API can change, each
//! toggled or edited in place and written with a `PUT` to its route.
//!
//! Deviations from upstream:
//! - Enter on an edit whose setting has gone (the config failed to load
//!   while it was open) closes the edit and does nothing more. Upstream's
//!   command panics there, indexing past its settings.

use std::sync::Arc;

use serde_json::Value;

use crate::app::Msg;
use crate::client::{Client, Object, get_string};
use crate::dashboard::{get_bool, get_float};
use crate::i18n::Locale;
use crate::keys::Key;
use crate::style::Style;
use crate::styles;
use crate::tea::Cmd;
use crate::textinput::TextInput;
use crate::viewport::Viewport;

/// What a setting holds, and so how it is changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Int,
    String,
    ReadOnly,
}

/// A setting (upstream's `configField`). Upstream's `rawValue` is always
/// nil, so it isn't here.
#[derive(Debug, Clone)]
struct Field {
    label: &'static str,
    api_path: &'static str,
    kind: Kind,
    value: String,
}

/// The result of a change (upstream's `configUpdateMsg`).
#[derive(Debug, Clone)]
pub(crate) struct Update {
    pub(crate) path: String,
    pub(crate) value: Option<Value>,
    pub(crate) err: Option<String>,
}

/// The config tab (upstream's `configTabModel`).
pub(crate) struct ConfigTab {
    client: Arc<Client>,
    locale: Locale,
    viewport: Viewport,
    fields: Vec<Field>,
    cursor: usize,
    editing: bool,
    text_input: TextInput,
    err: Option<String>,
    message: String,
    ready: bool,
}

impl ConfigTab {
    /// `newConfigTabModel`.
    pub(crate) fn new(client: Arc<Client>, locale: Locale) -> Self {
        let mut text_input = TextInput::new();
        text_input.char_limit = 256;
        Self {
            client,
            locale,
            viewport: Viewport::default(),
            fields: Vec::new(),
            cursor: 0,
            editing: false,
            text_input,
            err: None,
            message: String::new(),
            ready: false,
        }
    }

    /// `Init`.
    pub(crate) fn init(&self) -> Option<Cmd> {
        Some(self.fetch_config())
    }

    /// `fetchConfig`.
    fn fetch_config(&self) -> Cmd {
        let client = Arc::clone(&self.client);
        Cmd::run(async move {
            let config = client.get_config().await;
            Some(Msg::ConfigData(config.map(Option::unwrap_or_default)))
        })
    }

    /// `Update`.
    pub(crate) fn update(&mut self, msg: Msg) -> Option<Cmd> {
        match msg {
            Msg::LocaleChanged => {
                self.refresh();
                None
            }
            Msg::ConfigData(result) => {
                match result {
                    Err(err) => {
                        self.err = Some(err);
                        self.fields.clear();
                    }
                    Ok(cfg) => {
                        self.err = None;
                        self.fields = parse_config(&cfg);
                    }
                }
                self.refresh();
                None
            }
            Msg::ConfigUpdate(update) => {
                self.message = match update.err {
                    Some(err) => styles::ERROR.render(&format!("✗ {err}")),
                    None => styles::SUCCESS.render(self.locale.t("updated_ok")),
                };
                self.refresh();
                Some(self.fetch_config())
            }
            Msg::Key(key) => {
                if self.editing {
                    self.handle_editing_key(&key)
                } else {
                    self.handle_normal_key(&key)
                }
            }
            _ => None,
        }
    }

    fn refresh(&mut self) {
        let content = self.render_content();
        self.viewport.set_content(&content);
    }

    /// `handleNormalKey`.
    fn handle_normal_key(&mut self, key: &Key) -> Option<Cmd> {
        match key.string().as_str() {
            "r" => {
                self.message.clear();
                Some(self.fetch_config())
            }
            "up" | "k" => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.refresh();
                    self.ensure_cursor_visible();
                }
                None
            }
            "down" | "j" => {
                if self.cursor + 1 < self.fields.len() {
                    self.cursor += 1;
                    self.refresh();
                    self.ensure_cursor_visible();
                }
                None
            }
            "enter" | " " => {
                let field = self.fields.get(self.cursor)?.clone();
                match field.kind {
                    Kind::ReadOnly => None,
                    Kind::Bool => Some(self.toggle_bool(field)),
                    Kind::Int | Kind::String => {
                        self.editing = true;
                        self.text_input.set_value(&field.value);
                        self.text_input.focus();
                        self.refresh();
                        None
                    }
                }
            }
            _ => {
                self.viewport.update(key);
                None
            }
        }
    }

    /// `handleEditingKey`.
    fn handle_editing_key(&mut self, key: &Key) -> Option<Cmd> {
        match key.string().as_str() {
            "enter" => {
                self.editing = false;
                self.text_input.blur();
                let field = self.fields.get(self.cursor)?.clone();
                Some(self.submit_edit(field, self.text_input.value()))
            }
            "esc" => {
                self.editing = false;
                self.text_input.blur();
                self.refresh();
                None
            }
            _ => {
                self.text_input.update(key);
                self.refresh();
                None
            }
        }
    }

    /// `toggleBool`.
    fn toggle_bool(&self, field: Field) -> Cmd {
        let client = Arc::clone(&self.client);
        Cmd::run(async move {
            let new_value = field.value != "true";
            let result = client
                .put_field(field.api_path, Value::Bool(new_value))
                .await;
            Some(Msg::ConfigUpdate(Update {
                path: field.api_path.to_owned(),
                value: Some(Value::Bool(new_value)),
                err: result.err(),
            }))
        })
    }

    /// `submitEdit`.
    fn submit_edit(&self, field: Field, new_value: String) -> Cmd {
        let client = Arc::clone(&self.client);
        let locale = self.locale.clone();
        Cmd::run(async move {
            let (value, result) = match field.kind {
                Kind::Int => {
                    let Ok(n) = new_value.parse::<i64>() else {
                        return Some(Msg::ConfigUpdate(Update {
                            path: field.api_path.to_owned(),
                            value: None,
                            err: Some(format!("{}: {new_value}", locale.t("invalid_int"))),
                        }));
                    };
                    let value = Value::from(n);
                    (
                        Some(value.clone()),
                        client.put_field(field.api_path, value).await,
                    )
                }
                Kind::String => {
                    let value = Value::String(new_value);
                    (
                        Some(value.clone()),
                        client.put_field(field.api_path, value).await,
                    )
                }
                Kind::Bool | Kind::ReadOnly => (None, Ok(())),
            };
            Some(Msg::ConfigUpdate(Update {
                path: field.api_path.to_owned(),
                value,
                err: result.err(),
            }))
        })
    }

    /// `SetSize`.
    pub(crate) fn set_size(&mut self, width: i64, height: i64) {
        if self.ready {
            self.viewport.width = width;
            self.viewport.height = height;
        } else {
            self.viewport = Viewport::new(width, height);
            self.refresh();
            self.ready = true;
        }
    }

    /// `ensureCursorVisible`.
    fn ensure_cursor_visible(&mut self) {
        let target = i64::try_from(self.cursor)
            .unwrap_or(i64::MAX)
            .saturating_add(5);
        if target < self.viewport.y_offset() {
            self.viewport.set_y_offset(target);
        }
        if target >= self.viewport.y_offset() + self.viewport.height {
            self.viewport
                .set_y_offset(target - self.viewport.height + 1);
        }
    }

    /// `View`.
    pub(crate) fn view(&self) -> String {
        if !self.ready {
            return self.locale.t("loading").to_owned();
        }
        self.viewport.view()
    }

    /// `renderContent`.
    fn render_content(&self) -> String {
        let t = |key| self.locale.t(key);
        let mut sb = String::new();
        sb.push_str(&styles::TITLE.render(t("config_title")));
        sb.push('\n');
        if !self.message.is_empty() {
            sb.push_str("  ");
            sb.push_str(&self.message);
            sb.push('\n');
        }
        sb.push_str(&styles::HELP.render(t("config_help1")));
        sb.push('\n');
        sb.push_str(&styles::HELP.render(t("config_help2")));
        sb.push_str("\n\n");

        if let Some(err) = &self.err {
            sb.push_str(&styles::ERROR.render(&format!("  ⚠ Error: {err}")));
            return sb;
        }
        if self.fields.is_empty() {
            sb.push_str(&styles::SUBTITLE.render(t("no_config")));
            return sb;
        }

        let mut current_section = "";
        for (i, field) in self.fields.iter().enumerate() {
            let section = self.field_section(field.api_path);
            if section != current_section {
                current_section = section;
                sb.push('\n');
                sb.push_str(
                    &Style::new()
                        .bold(true)
                        .fg(styles::COLOR_HIGHLIGHT)
                        .render(&format!("  ── {section} ")),
                );
                sb.push('\n');
            }

            let selected = i == self.cursor;
            let prefix = if selected { "▸ " } else { "  " };
            let label = Style::new()
                .fg(styles::COLOR_INFO)
                .bold(selected)
                .width(32)
                .render(field.label);
            let value = if self.editing && selected {
                self.text_input.view()
            } else {
                match field.kind {
                    Kind::Bool if field.value == "true" => styles::SUCCESS.render("● ON"),
                    Kind::Bool => Style::new().fg(styles::COLOR_MUTED).render("○ OFF"),
                    Kind::ReadOnly => Style::new().fg(styles::COLOR_SUBTEXT).render(&field.value),
                    Kind::Int | Kind::String => styles::VALUE.render(&field.value),
                }
            };
            let mut line = format!("{prefix}{label}  {value}");
            if selected && !self.editing {
                line = Style::new().bg(styles::COLOR_SURFACE).render(&line);
            }
            sb.push_str(&line);
            sb.push('\n');
        }
        sb
    }

    /// `fieldSection`.
    fn field_section(&self, api_path: &str) -> &'static str {
        let key = if api_path.starts_with("quota-exceeded/") {
            "section_quota"
        } else if api_path.starts_with("routing/") {
            "section_routing"
        } else {
            match api_path {
                "port" | "host" | "debug" | "proxy-url" | "request-retry"
                | "max-retry-interval" | "force-model-prefix" => "section_server",
                "logging-to-file"
                | "logs-max-total-size-mb"
                | "error-logs-max-files"
                | "usage-statistics-enabled"
                | "request-log" => "section_logging",
                "ws-auth" => "section_websocket",
                _ => "section_other",
            }
        };
        self.locale.t(key)
    }
}

/// `parseConfig`.
fn parse_config(cfg: &Object) -> Vec<Field> {
    let field = |label, api_path, kind, value| Field {
        label,
        api_path,
        kind,
        value,
    };
    let num = |key| format!("{:.0}", get_float(cfg, key));
    let flag = |key| get_bool(cfg, key).to_string();
    let nested = |outer, key| get_bool_nested(cfg, outer, key).to_string();
    let strategy = match cfg.get("routing") {
        Some(Value::Object(routing)) => get_string(routing, "strategy"),
        _ => String::new(),
    };
    vec![
        field("Port", "port", Kind::ReadOnly, num("port")),
        field("Host", "host", Kind::ReadOnly, get_string(cfg, "host")),
        field("Debug", "debug", Kind::Bool, flag("debug")),
        field(
            "Proxy URL",
            "proxy-url",
            Kind::String,
            get_string(cfg, "proxy-url"),
        ),
        field(
            "Request Retry",
            "request-retry",
            Kind::Int,
            num("request-retry"),
        ),
        field(
            "Max Retry Interval (s)",
            "max-retry-interval",
            Kind::Int,
            num("max-retry-interval"),
        ),
        field(
            "Force Model Prefix",
            "force-model-prefix",
            Kind::String,
            get_string(cfg, "force-model-prefix"),
        ),
        field(
            "Logging to File",
            "logging-to-file",
            Kind::Bool,
            flag("logging-to-file"),
        ),
        field(
            "Logs Max Total Size (MB)",
            "logs-max-total-size-mb",
            Kind::Int,
            num("logs-max-total-size-mb"),
        ),
        field(
            "Error Logs Max Files",
            "error-logs-max-files",
            Kind::Int,
            num("error-logs-max-files"),
        ),
        field(
            "Usage Stats Enabled",
            "usage-statistics-enabled",
            Kind::Bool,
            flag("usage-statistics-enabled"),
        ),
        field(
            "Request Log",
            "request-log",
            Kind::Bool,
            flag("request-log"),
        ),
        field(
            "Switch Project on Quota",
            "quota-exceeded/switch-project",
            Kind::Bool,
            nested("quota-exceeded", "switch-project"),
        ),
        field(
            "Switch Preview Model",
            "quota-exceeded/switch-preview-model",
            Kind::Bool,
            nested("quota-exceeded", "switch-preview-model"),
        ),
        field(
            "Routing Strategy",
            "routing/strategy",
            Kind::String,
            strategy,
        ),
        field("WebSocket Auth", "ws-auth", Kind::Bool, flag("ws-auth")),
    ]
}

/// `getBoolNested` for two keys.
fn get_bool_nested(m: &Object, outer: &str, key: &str) -> bool {
    match m.get(outer) {
        Some(Value::Object(nested)) => get_bool(nested, key),
        _ => false,
    }
}
