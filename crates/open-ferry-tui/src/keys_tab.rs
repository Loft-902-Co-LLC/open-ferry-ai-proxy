// Ported from CLIProxyAPI internal/tui/keys_tab.go (newKeysTabModel, Init,
// fetchKeys, Update, SetSize, View, renderContent, renderSection,
// renderProviderKeys, maskKey) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The API keys tab: the server's access keys, which can be added, edited,
//! deleted and copied, and its providers' keys, shown masked.
//!
//! As upstream's does, editing an access key puts the whole key in the
//! input.
//!
//! Deviations from upstream:
//! - `c` copies the key by asking the terminal to (OSC 52), which works
//!   over SSH and in most terminals; a terminal that doesn't support it
//!   ignores the request, and the tab still says the key was copied.
//!   Upstream writes to the system clipboard itself.
//! - A key is masked by characters: up to 8 become as many `*`s, longer
//!   ones keep their first and last 4. Upstream counts and slices bytes,
//!   which can split a character; the two agree on ASCII.

use std::fmt::Write as _;
use std::sync::Arc;

use crate::app::{Msg, Platform};
use crate::client::{Client, Object, get_string};
use crate::i18n::{Locale, fill};
use crate::keys::Key;
use crate::style::Style;
use crate::styles;
use crate::tea::Cmd;
use crate::textinput::TextInput;
use crate::viewport::Viewport;

/// The provider key lists the tab shows, read-only: title and route.
const PROVIDER_KEYS: [(&str, &str); 6] = [
    ("Gemini API Keys", "gemini-api-key"),
    ("Interactions API Keys", "interactions-api-key"),
    ("Claude API Keys", "claude-api-key"),
    ("Codex API Keys", "codex-api-key"),
    ("xAI API Keys", "xai-api-key"),
    ("Vertex API Keys", "vertex-api-key"),
];

/// What [`KeysTab`] fetched (upstream's `keysDataMsg` without an error).
#[derive(Debug, Clone, Default)]
pub(crate) struct Data {
    api_keys: Vec<String>,
    /// The lists in [`PROVIDER_KEYS`] order.
    providers: [Vec<Object>; 6],
    openai: Vec<Object>,
}

/// The API keys tab (upstream's `keysTabModel`).
pub(crate) struct KeysTab {
    client: Arc<Client>,
    locale: Locale,
    platform: Platform,
    viewport: Viewport,
    data: Data,
    err: Option<String>,
    width: i64,
    ready: bool,
    cursor: usize,
    confirm: Option<usize>,
    status: String,
    editing: bool,
    adding: bool,
    edit_idx: usize,
    edit_input: TextInput,
}

impl KeysTab {
    /// `newKeysTabModel`.
    pub(crate) fn new(client: Arc<Client>, locale: Locale, platform: Platform) -> Self {
        let mut edit_input = TextInput::new();
        edit_input.char_limit = 512;
        edit_input.prompt = "  Key: ".to_owned();
        Self {
            client,
            locale,
            platform,
            viewport: Viewport::default(),
            data: Data::default(),
            err: None,
            width: 0,
            ready: false,
            cursor: 0,
            confirm: None,
            status: String::new(),
            editing: false,
            adding: false,
            edit_idx: 0,
            edit_input,
        }
    }

    /// `Init`.
    pub(crate) fn init(&self) -> Option<Cmd> {
        Some(self.fetch_keys())
    }

    /// `fetchKeys`: the access keys, then each provider's, whose errors
    /// leave them empty.
    fn fetch_keys(&self) -> Cmd {
        let client = Arc::clone(&self.client);
        Cmd::run(async move {
            let api_keys = match client.get_api_keys().await {
                Ok(keys) => keys,
                Err(err) => return Some(Msg::KeysData(Box::new(Err(err)))),
            };
            let mut data = Data {
                api_keys,
                ..Data::default()
            };
            for (list, (_, route)) in data.providers.iter_mut().zip(PROVIDER_KEYS) {
                *list = client.get_key_list(route).await.unwrap_or_default();
            }
            data.openai = client
                .get_key_list("openai-compatibility")
                .await
                .unwrap_or_default();
            Some(Msg::KeysData(Box::new(Ok(data))))
        })
    }

    /// `Update`.
    pub(crate) fn update(&mut self, msg: Msg) -> Option<Cmd> {
        match msg {
            Msg::LocaleChanged => {
                self.refresh();
                None
            }
            Msg::KeysData(result) => {
                match *result {
                    Err(err) => self.err = Some(err),
                    Ok(data) => {
                        self.err = None;
                        self.data = data;
                        if self.cursor >= self.data.api_keys.len() {
                            self.cursor = self.data.api_keys.len().saturating_sub(1);
                        }
                    }
                }
                self.refresh();
                None
            }
            Msg::KeyAction(result) => {
                self.status = match result {
                    Err(err) => styles::ERROR.render(&format!("✗ {err}")),
                    Ok(action) => styles::SUCCESS.render(&format!("✓ {action}")),
                };
                self.confirm = None;
                self.refresh();
                Some(self.fetch_keys())
            }
            Msg::Key(key) => {
                if self.editing || self.adding {
                    self.handle_edit_key(&key)
                } else if self.confirm.is_some() {
                    self.handle_confirm_key(&key)
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

    fn handle_edit_key(&mut self, key: &Key) -> Option<Cmd> {
        match key.string().as_str() {
            "enter" => {
                let value = self.edit_input.value().trim().to_owned();
                let adding = self.adding;
                let edit_idx = self.edit_idx;
                self.editing = false;
                self.adding = false;
                self.edit_input.blur();
                if value.is_empty() {
                    self.refresh();
                    return None;
                }
                let client = Arc::clone(&self.client);
                let locale = self.locale.clone();
                Some(Cmd::run(async move {
                    let result = if adding {
                        client.add_api_key(&value).await.map(|()| "key_added")
                    } else {
                        client
                            .edit_api_key(edit_idx, &value)
                            .await
                            .map(|()| "key_updated")
                    };
                    Some(Msg::KeyAction(
                        result.map(|action| locale.t(action).to_owned()),
                    ))
                }))
            }
            "esc" => {
                self.editing = false;
                self.adding = false;
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

    fn handle_confirm_key(&mut self, key: &Key) -> Option<Cmd> {
        match key.string().as_str() {
            "y" | "Y" => {
                let idx = self.confirm.take()?;
                let client = Arc::clone(&self.client);
                let locale = self.locale.clone();
                Some(Cmd::run(async move {
                    let result = client.delete_api_key(idx).await;
                    Some(Msg::KeyAction(
                        result.map(|()| locale.t("key_deleted").to_owned()),
                    ))
                }))
            }
            "n" | "N" | "esc" => {
                self.confirm = None;
                self.refresh();
                None
            }
            _ => None,
        }
    }

    fn handle_normal_key(&mut self, key: &Key) -> Option<Cmd> {
        let count = self.data.api_keys.len();
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
            "a" => {
                self.adding = true;
                self.editing = false;
                self.edit_input.set_value("");
                self.edit_input.prompt = self.locale.t("new_key_prompt").to_owned();
                self.edit_input.focus();
                self.refresh();
                None
            }
            "e" => {
                let current = self.data.api_keys.get(self.cursor)?.clone();
                self.editing = true;
                self.adding = false;
                self.edit_idx = self.cursor;
                self.edit_input.set_value(&current);
                self.edit_input.prompt = self.locale.t("edit_key_prompt").to_owned();
                self.edit_input.focus();
                self.refresh();
                None
            }
            "d" => {
                if self.cursor < count {
                    self.confirm = Some(self.cursor);
                    self.refresh();
                }
                None
            }
            "c" => {
                if let Some(current) = self.data.api_keys.get(self.cursor) {
                    self.status = match (self.platform.copy)(current) {
                        Ok(()) => styles::SUCCESS.render(self.locale.t("copied")),
                        Err(err) => styles::ERROR
                            .render(&format!("{}: {err}", self.locale.t("copy_failed"))),
                    };
                    self.refresh();
                }
                None
            }
            "r" => {
                self.status.clear();
                Some(self.fetch_keys())
            }
            _ => {
                self.viewport.update(key);
                None
            }
        }
    }

    /// `SetSize`.
    pub(crate) fn set_size(&mut self, width: i64, height: i64) {
        self.width = width;
        self.edit_input.width = width - 16;
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
        sb.push_str(&styles::TITLE.render(t("keys_title")));
        sb.push('\n');
        sb.push_str(&styles::HELP.render(t("keys_help")));
        sb.push('\n');
        sb.push_str(&styles::rule(self.width));
        sb.push('\n');

        if let Some(err) = &self.err {
            sb.push_str(&styles::ERROR.render(&format!("{}{err}", t("error_prefix"))));
            sb.push('\n');
            return sb;
        }

        let keys = &self.data.api_keys;
        sb.push_str(&styles::TABLE_HEADER.render(&format!(
            "  {} ({})",
            t("access_keys"),
            keys.len()
        )));
        sb.push('\n');
        if keys.is_empty() {
            sb.push_str(&styles::SUBTITLE.render(t("no_keys")));
            sb.push('\n');
        }
        for (i, key) in keys.iter().enumerate() {
            let (cursor, row_style) = if i == self.cursor {
                ("▸ ", Style::new().bold(true))
            } else {
                ("  ", Style::new())
            };
            sb.push_str(&row_style.render(&format!("{cursor}{}. {}", i + 1, mask_key(key))));
            sb.push('\n');
            if self.confirm == Some(i) {
                let text = format!("    {}", t("confirm_delete_key"));
                sb.push_str(&styles::WARNING.render(&fill(&text, &[&mask_key(key)])));
                sb.push('\n');
            }
            if self.editing && self.edit_idx == i {
                sb.push_str(&self.edit_input.view());
                sb.push('\n');
                sb.push_str(&styles::HELP.render(t("enter_save_esc")));
                sb.push('\n');
            }
        }
        if self.adding {
            sb.push('\n');
            sb.push_str(&self.edit_input.view());
            sb.push('\n');
            sb.push_str(&styles::HELP.render(t("enter_add")));
            sb.push('\n');
        }
        sb.push('\n');

        for ((title, _), list) in PROVIDER_KEYS.iter().zip(&self.data.providers) {
            render_provider_keys(&mut sb, title, list);
        }

        if !self.data.openai.is_empty() {
            render_section(&mut sb, "OpenAI Compatibility", self.data.openai.len());
            for (i, entry) in self.data.openai.iter().enumerate() {
                let mut info = get_string(entry, "name");
                let base_url = get_string(entry, "base-url");
                let prefix = get_string(entry, "prefix");
                if !prefix.is_empty() {
                    let _ = write!(info, " (prefix: {prefix})");
                }
                if !base_url.is_empty() {
                    let _ = write!(info, " → {base_url}");
                }
                let _ = writeln!(sb, "  {}. {info}", i + 1);
            }
            sb.push('\n');
        }

        if !self.status.is_empty() {
            sb.push_str(&self.status);
            sb.push('\n');
        }
        sb
    }
}

/// `renderSection`.
fn render_section(sb: &mut String, title: &str, count: usize) {
    sb.push_str(&styles::TABLE_HEADER.render(&format!("  {title} ({count})")));
    sb.push('\n');
}

/// `renderProviderKeys`.
fn render_provider_keys(sb: &mut String, title: &str, keys: &[Object]) {
    if keys.is_empty() {
        return;
    }
    render_section(sb, title, keys.len());
    for (i, key) in keys.iter().enumerate() {
        let mut info = mask_key(&get_string(key, "api-key"));
        let prefix = get_string(key, "prefix");
        let base_url = get_string(key, "base-url");
        if !prefix.is_empty() {
            let _ = write!(info, " (prefix: {prefix})");
        }
        if !base_url.is_empty() {
            let _ = write!(info, " → {base_url}");
        }
        let _ = writeln!(sb, "  {}. {info}", i + 1);
    }
    sb.push('\n');
}

/// `maskKey`: up to 8 characters become `*`s; a longer key keeps its first
/// and last 4.
pub(crate) fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let n = chars.len();
    if n <= 8 {
        return "*".repeat(n);
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars.iter().skip(n - 4).collect();
    format!("{head}{}{tail}", "*".repeat(n - 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: keys mask as upstream's maskKey masks them.
    #[test]
    fn masks_keys_as_upstream_does() {
        assert_eq!(mask_key(""), "");
        assert_eq!(mask_key("short"), "*****");
        assert_eq!(mask_key("12345678"), "********");
        assert_eq!(mask_key("abcdefghij"), "abcd**ghij");
        assert_eq!(mask_key("sk-test-1234567890"), "sk-t**********7890");
        assert_eq!(mask_key("äöüßäöüßä"), "äöüß*öüßä");
    }
}
