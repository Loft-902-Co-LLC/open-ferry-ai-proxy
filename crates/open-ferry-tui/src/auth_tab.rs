// Ported from CLIProxyAPI internal/tui/auth_tab.go (newAuthTabModel, Init,
// fetchFiles, Update, startEdit, SetSize, View, renderContent, renderDetail,
// getAnyString, handleEditInput, handleConfirmInput, handleNormalInput)
// (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The auth files tab: the server's credential files, each shown in detail,
//! enabled or disabled, deleted, refreshed, or given a prefix, proxy URL or
//! priority.
//!
//! Deviations from upstream:
//! - A name longer than 24 characters, or an email longer than 28, is cut
//!   by characters. Upstream counts and cuts bytes, which can split a
//!   character; the two agree on ASCII.

use std::fmt::Write as _;
use std::sync::Arc;

use serde_json::Value;

use crate::app::Msg;
use crate::client::{Client, Object, get_string};
use crate::dashboard::get_bool;
use crate::i18n::{Locale, fill};
use crate::keys::Key;
use crate::style::{Color, Style};
use crate::styles;
use crate::tea::Cmd;
use crate::textinput::TextInput;
use crate::viewport::Viewport;

/// The fields an auth file's detail lets the user edit (upstream's
/// `authEditableFields`): label and key.
const EDITABLE_FIELDS: [(&str, &str); 3] = [
    ("Prefix", "prefix"),
    ("Proxy URL", "proxy_url"),
    ("Priority", "priority"),
];

/// The fields an auth file's detail shows: label, key, and whether it is
/// editable.
const DETAIL_FIELDS: [(&str, &str, bool); 14] = [
    ("Name", "name", false),
    ("Channel", "channel", false),
    ("Email", "email", false),
    ("Status", "status", false),
    ("Status Msg", "status_message", false),
    ("File Name", "file_name", false),
    ("Auth Type", "auth_type", false),
    ("Prefix", "prefix", true),
    ("Proxy URL", "proxy_url", true),
    ("Priority", "priority", true),
    ("Project ID", "project_id", false),
    ("Disabled", "disabled", false),
    ("Created", "created_at", false),
    ("Updated", "updated_at", false),
];

/// The auth files tab (upstream's `authTabModel`).
pub(crate) struct AuthTab {
    client: Arc<Client>,
    locale: Locale,
    viewport: Viewport,
    files: Vec<Object>,
    err: Option<String>,
    width: i64,
    ready: bool,
    cursor: usize,
    expanded: Option<usize>,
    confirm: Option<usize>,
    status: String,
    editing: bool,
    edit_field: usize,
    edit_input: TextInput,
    edit_file_name: String,
}

impl AuthTab {
    /// `newAuthTabModel`.
    pub(crate) fn new(client: Arc<Client>, locale: Locale) -> Self {
        let mut edit_input = TextInput::new();
        edit_input.char_limit = 256;
        Self {
            client,
            locale,
            viewport: Viewport::default(),
            files: Vec::new(),
            err: None,
            width: 0,
            ready: false,
            cursor: 0,
            expanded: None,
            confirm: None,
            status: String::new(),
            editing: false,
            edit_field: 0,
            edit_input,
            edit_file_name: String::new(),
        }
    }

    /// `Init`.
    pub(crate) fn init(&self) -> Option<Cmd> {
        Some(self.fetch_files())
    }

    /// `fetchFiles`.
    fn fetch_files(&self) -> Cmd {
        let client = Arc::clone(&self.client);
        Cmd::run(async move { Some(Msg::AuthFiles(client.get_auth_files().await)) })
    }

    /// `Update`.
    pub(crate) fn update(&mut self, msg: Msg) -> Option<Cmd> {
        match msg {
            Msg::LocaleChanged => {
                self.refresh();
                None
            }
            Msg::AuthFiles(result) => {
                match result {
                    Err(err) => self.err = Some(err),
                    Ok(files) => {
                        self.err = None;
                        self.files = files;
                        if self.cursor >= self.files.len() {
                            self.cursor = self.files.len().saturating_sub(1);
                        }
                        self.status.clear();
                    }
                }
                self.refresh();
                None
            }
            Msg::AuthAction(result) => {
                self.status = match result {
                    Err(err) => styles::ERROR.render(&format!("✗ {err}")),
                    Ok(action) => styles::SUCCESS.render(&format!("✓ {action}")),
                };
                self.confirm = None;
                self.refresh();
                Some(self.fetch_files())
            }
            Msg::Key(key) => {
                if self.editing {
                    self.handle_edit_input(&key)
                } else if self.confirm.is_some() {
                    self.handle_confirm_input(&key)
                } else {
                    self.handle_normal_input(&key)
                }
            }
            _ => None,
        }
    }

    fn refresh(&mut self) {
        let content = self.render_content();
        self.viewport.set_content(&content);
    }

    /// `startEdit`.
    fn start_edit(&mut self, field_idx: usize) -> Option<Cmd> {
        let file = self.files.get(self.cursor)?;
        let (label, key) = *EDITABLE_FIELDS.get(field_idx)?;
        self.edit_file_name = get_string(file, "name");
        let current = get_any_string(file, key);
        self.edit_field = field_idx;
        self.editing = true;
        self.edit_input.set_value(&current);
        self.edit_input.focus();
        self.edit_input.prompt = format!("  {label}: ");
        self.refresh();
        None
    }

    /// `SetSize`.
    pub(crate) fn set_size(&mut self, width: i64, height: i64) {
        self.width = width;
        self.edit_input.width = width - 20;
        if self.ready {
            self.viewport.width = width;
            self.viewport.height = height;
        } else {
            self.viewport = Viewport::new(width, height);
            self.refresh();
            self.ready = true;
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
        sb.push_str(&styles::TITLE.render(t("auth_title")));
        sb.push('\n');
        sb.push_str(&styles::HELP.render(t("auth_help1")));
        sb.push('\n');
        sb.push_str(&styles::HELP.render(t("auth_help2")));
        sb.push('\n');
        sb.push_str(&styles::rule(self.width));
        sb.push('\n');

        if let Some(err) = &self.err {
            sb.push_str(&styles::ERROR.render(&format!("⚠ Error: {err}")));
            sb.push('\n');
            return sb;
        }
        if self.files.is_empty() {
            sb.push_str(&styles::SUBTITLE.render(t("no_auth_files")));
            sb.push('\n');
            return sb;
        }

        for (i, file) in self.files.iter().enumerate() {
            let name = get_string(file, "name");
            let channel = get_string(file, "channel");
            let email = get_string(file, "email");
            let (icon, status_text) = if get_bool(file, "disabled") {
                (
                    Style::new().fg(styles::COLOR_MUTED).render("○"),
                    t("status_disabled"),
                )
            } else {
                (styles::SUCCESS.render("●"), t("status_active"))
            };
            let (cursor, row_style) = if i == self.cursor {
                ("▸ ", Style::new().bold(true))
            } else {
                ("  ", Style::new())
            };
            let row = format!(
                "{cursor}{icon} {} {} {} {status_text}",
                pad(&cut(&name, 24), 24),
                pad(&channel, 12),
                pad(&cut(&email, 28), 28),
            );
            sb.push_str(&row_style.render(&row));
            sb.push('\n');

            if self.confirm == Some(i) {
                let text = format!("    {}", t("confirm_delete"));
                sb.push_str(&styles::WARNING.render(&fill(&text, &[&name])));
                sb.push('\n');
            }
            if self.editing && i == self.cursor {
                sb.push_str(&self.edit_input.view());
                sb.push('\n');
                sb.push_str(&styles::HELP.render(&format!(
                    "    {} • {}",
                    t("enter_save"),
                    t("esc_cancel")
                )));
                sb.push('\n');
            }
            if self.expanded == Some(i) {
                sb.push_str(&self.render_detail(file));
            }
        }

        if !self.status.is_empty() {
            sb.push('\n');
            sb.push_str(&self.status);
            sb.push('\n');
        }
        sb
    }

    /// `renderDetail`.
    fn render_detail(&self, file: &Object) -> String {
        let label_style = Style::new().fg(Color::Ansi(111)).bold(true);
        let value_style = Style::new().fg(Color::Ansi(252));
        let edit_marker = Style::new().fg(Color::Ansi(214)).render(" ✎");

        let mut sb = String::from("    ┌─────────────────────────────────────────────\n");
        for (label, key, editable) in DETAIL_FIELDS {
            let mut val = get_any_string(file, key);
            if val.is_empty() || val == "<nil>" {
                if editable {
                    val = self.locale.t("not_set").to_owned();
                } else {
                    continue;
                }
            }
            let mark = if editable { edit_marker.as_str() } else { "" };
            let _ = writeln!(
                sb,
                "    │ {} {}{mark}",
                label_style.render(&format!("{}:", pad(label, 12))),
                value_style.render(&val),
            );
        }
        sb.push_str("    └─────────────────────────────────────────────\n");
        sb
    }

    /// `handleEditInput`.
    fn handle_edit_input(&mut self, key: &Key) -> Option<Cmd> {
        match key.string().as_str() {
            "enter" => {
                let value = self.edit_input.value();
                let (_, field_key) = *EDITABLE_FIELDS.get(self.edit_field)?;
                let file_name = self.edit_file_name.clone();
                self.editing = false;
                self.edit_input.blur();
                let locale = self.locale.clone();
                let mut fields = Object::new();
                if field_key == "priority" {
                    let Ok(priority) = value.parse::<i64>() else {
                        return Some(Cmd::run(async move {
                            Some(Msg::AuthAction(Err(format!(
                                "{}: {value}",
                                locale.t("invalid_int")
                            ))))
                        }));
                    };
                    fields.insert(field_key.to_owned(), Value::from(priority));
                } else {
                    fields.insert(field_key.to_owned(), Value::String(value));
                }
                let client = Arc::clone(&self.client);
                Some(Cmd::run(async move {
                    let result = client.patch_auth_file_fields(&file_name, fields).await;
                    Some(Msg::AuthAction(result.map(|()| {
                        fill(locale.t("updated_field"), &[field_key, file_name.as_str()])
                    })))
                }))
            }
            "esc" => {
                self.editing = false;
                self.edit_input.blur();
                self.refresh();
                None
            }
            _ => {
                self.edit_input.update(key);
                self.refresh();
                None
            }
        }
    }

    /// `handleConfirmInput`.
    fn handle_confirm_input(&mut self, key: &Key) -> Option<Cmd> {
        match key.string().as_str() {
            "y" | "Y" => {
                let idx = self.confirm.take()?;
                if let Some(file) = self.files.get(idx) {
                    let name = get_string(file, "name");
                    let client = Arc::clone(&self.client);
                    let locale = self.locale.clone();
                    return Some(Cmd::run(async move {
                        let result = client.delete_auth_file(&name).await;
                        Some(Msg::AuthAction(
                            result.map(|()| fill(locale.t("deleted"), &[&name])),
                        ))
                    }));
                }
                self.refresh();
                None
            }
            "n" | "N" | "esc" => {
                self.confirm = None;
                self.refresh();
                None
            }
            _ => None,
        }
    }

    /// `handleNormalInput`.
    fn handle_normal_input(&mut self, key: &Key) -> Option<Cmd> {
        let count = self.files.len();
        match key.string().as_str() {
            "j" | "down" => {
                if count > 0 {
                    self.cursor = (self.cursor + 1) % count;
                    self.refresh();
                }
                None
            }
            "k" | "up" => {
                if count > 0 {
                    self.cursor = (self.cursor + count - 1) % count;
                    self.refresh();
                }
                None
            }
            "enter" | " " => {
                self.expanded = if self.expanded == Some(self.cursor) {
                    None
                } else {
                    Some(self.cursor)
                };
                self.refresh();
                None
            }
            "d" | "D" => {
                if self.cursor < count {
                    self.confirm = Some(self.cursor);
                    self.refresh();
                }
                None
            }
            "e" | "E" => {
                let file = self.files.get(self.cursor)?;
                let name = get_string(file, "name");
                let new_disabled = !get_bool(file, "disabled");
                let client = Arc::clone(&self.client);
                let locale = self.locale.clone();
                Some(Cmd::run(async move {
                    let result = client.toggle_auth_file(&name, new_disabled).await;
                    Some(Msg::AuthAction(result.map(|()| {
                        let action = locale.t(if new_disabled { "disabled" } else { "enabled" });
                        format!("{action} {name}")
                    })))
                }))
            }
            "1" => self.start_edit(0),
            "2" => self.start_edit(1),
            "3" => self.start_edit(2),
            "r" => {
                self.status.clear();
                Some(self.fetch_files())
            }
            "R" => {
                let file = self.files.get(self.cursor)?;
                let name = get_string(file, "name");
                let client = Arc::clone(&self.client);
                let locale = self.locale.clone();
                Some(Cmd::run(async move {
                    let result = client.refresh_auth_file(&name).await;
                    Some(Msg::AuthAction(
                        result.map(|()| fill(locale.t("refreshed_auth"), &[&name])),
                    ))
                }))
            }
            _ => {
                self.viewport.update(key);
                None
            }
        }
    }
}

/// `getAnyString`: the value under `key` as `%v` prints it; "" when it is
/// missing or null.
pub(crate) fn get_any_string(m: &Object, key: &str) -> String {
    match m.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(v) => go_v(v),
    }
}

/// A JSON value as Go's `%v` prints it once decoded into `any`.
fn go_v(v: &Value) -> String {
    match v {
        Value::Null => "<nil>".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => open_ferry_translate::go::format_float_g(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(go_v).collect();
            format!("[{}]", items.join(" "))
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let entries: Vec<String> = entries
                .into_iter()
                .map(|(k, v)| format!("{k}:{}", go_v(v)))
                .collect();
            format!("map[{}]", entries.join(" "))
        }
    }
}

/// `s` cut to `max - 3` characters and `...` when longer than `max`.
fn cut(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
        out.push_str("...");
        out
    } else {
        s.to_owned()
    }
}

/// `s` padded with spaces to `width` characters, as `%-Ns` pads.
pub(crate) fn pad(s: &str, width: usize) -> String {
    let count = s.chars().count();
    let mut out = s.to_owned();
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(count)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: values print as Go's %v prints them.
    #[test]
    fn prints_values_as_go_does() {
        let file: Object = serde_json::from_str(
            r#"{"a":1.5,"b":2,"c":true,"d":null,"e":"x","f":[1,"y"],"g":{"z":1,"k":false},"h":1e21}"#,
        )
        .unwrap();
        let got: Vec<String> = ["a", "b", "c", "d", "e", "f", "g", "h", "missing"]
            .iter()
            .map(|k| get_any_string(&file, k))
            .collect();
        assert_eq!(
            got,
            [
                "1.5",
                "2",
                "true",
                "",
                "x",
                "[1 y]",
                "map[k:false z:1]",
                "1e+21",
                ""
            ]
        );
        assert_eq!(cut("abcdefgh", 6), "abc...");
        assert_eq!(cut("äöüäöüäö", 6), "äöü...");
        assert_eq!(cut("abc", 6), "abc");
        assert_eq!(pad("äb", 4), "äb  ");
    }
}
