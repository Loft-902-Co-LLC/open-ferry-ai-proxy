// Ported from CLIProxyAPI internal/tui/app.go (NewApp, NewAppWithBaseURL,
// Init, Update, refreshTabs, initTabIfNeeded, View, renderAuthView,
// renderTabBar, renderStatusBar, fitStringWidth, isLogsEnabledFromConfig,
// setAuthInputPrompt, connectWithPassword, broadcastToAllTabs) (v8.0.15,
// MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The app: a bar of tabs over the active tab, and a status bar. In client
//! mode it first asks for the management key, and checks it by fetching
//! the config; in standalone mode it starts signed in to the embedded
//! server. The logs tab is shown only when the server logs to a file, or
//! in standalone mode.
//!
//! As upstream's does, the app takes `q` (quit) and `L` (switch language)
//! before the key gate's input or a tab's input sees them, so neither can
//! be typed there; and a tab's data reaches it only while it is the active
//! tab.
//!
//! Deviations from upstream:
//! - The language is the app's own, starting in English, rather than one
//!   for the whole process.

use std::sync::Arc;

use serde_json::Value;

use crate::auth_tab::AuthTab;
use crate::client::{Client, Object};
use crate::config_tab::{self, ConfigTab};
use crate::dashboard::{self, Dashboard};
use crate::i18n::{Locale, fill};
use crate::keys::Key;
use crate::keys_tab::{self, KeysTab};
use crate::loghook::LogHook;
use crate::logs_tab::LogsTab;
use crate::oauth_tab::{self, OAuthTab};
use crate::styles;
use crate::tea::{Cmd, batch};
use crate::textinput::TextInput;

const TAB_DASHBOARD: usize = 0;
const TAB_CONFIG: usize = 1;
const TAB_AUTH_FILES: usize = 2;
const TAB_API_KEYS: usize = 3;
const TAB_OAUTH: usize = 4;
const TAB_LOGS: usize = 5;

/// A message: input, or a command's result.
#[derive(Debug)]
pub(crate) enum Msg {
    /// A key press (`tea.KeyMsg`).
    Key(Key),
    /// The terminal's size (`tea.WindowSizeMsg`).
    Resize { width: i64, height: i64 },
    /// The config the key gate fetched (`authConnectMsg`).
    AuthConnect(Result<Option<Object>, String>),
    /// The language changed (`localeChangedMsg`).
    LocaleChanged,
    /// `dashboardDataMsg`.
    Dashboard(dashboard::Data),
    /// `configDataMsg`.
    ConfigData(Result<Object, String>),
    /// `configUpdateMsg`.
    ConfigUpdate(config_tab::Update),
    /// `authFilesMsg`.
    AuthFiles(Result<Vec<Object>, String>),
    /// `authActionMsg`.
    AuthAction(Result<String, String>),
    /// `keysDataMsg`.
    KeysData(Box<Result<keys_tab::Data, String>>),
    /// `keyActionMsg`.
    KeyAction(Result<String, String>),
    /// `oauthStartMsg`.
    OAuthStart(oauth_tab::Start),
    /// `oauthPollMsg`.
    OAuthPoll(oauth_tab::Poll),
    /// `oauthCallbackSubmitMsg`.
    OAuthCallbackSubmit(Result<(), String>),
    /// `logsPollMsg`: the lines and the latest one's timestamp.
    LogsPoll(Result<(Vec<String>, i64), String>),
    /// `logsTickMsg`.
    LogsTick,
    /// `logLineMsg`.
    LogLine(String),
}

/// Copies text, or says why it couldn't.
pub(crate) type CopyFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// What the app asks of the system: opening a URL in a browser, and
/// copying text. Tests record these instead.
#[derive(Clone)]
pub(crate) struct Platform {
    /// Opens a URL, best effort (upstream's `openBrowser`).
    pub(crate) open_url: Arc<dyn Fn(&str) + Send + Sync>,
    /// Copies text (upstream's `clipboard.WriteAll`).
    pub(crate) copy: CopyFn,
}

impl Platform {
    /// A platform that does nothing.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self {
            open_url: Arc::new(|_| {}),
            copy: Arc::new(|_| Ok(())),
        }
    }
}

impl std::fmt::Debug for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Platform")
    }
}

/// The app (upstream's `App`).
pub(crate) struct App {
    locale: Locale,
    active_tab: usize,
    tabs: Vec<&'static str>,
    standalone: bool,
    logs_enabled: bool,
    authenticated: bool,
    auth_input: TextInput,
    auth_error: String,
    auth_connecting: bool,
    dashboard: Dashboard,
    config: ConfigTab,
    auth: AuthTab,
    keys: KeysTab,
    oauth: OAuthTab,
    logs: LogsTab,
    client: Arc<Client>,
    width: i64,
    height: i64,
    ready: bool,
    /// Which tabs have fetched their data.
    initialized: [bool; 6],
}

impl App {
    /// `NewAppWithBaseURL`: an app for the server at `base_url`. With a
    /// hook, it is in standalone mode, signed in with `secret`; otherwise
    /// the key gate starts with `secret` typed in.
    pub(crate) fn new(
        base_url: &str,
        secret: &str,
        hook: Option<LogHook>,
        platform: Platform,
    ) -> Self {
        let locale = Locale::default();
        let standalone = hook.is_some();
        let auth_required = !standalone;
        let mut auth_input = TextInput::new();
        auth_input.char_limit = 512;
        auth_input.password = true;
        auth_input.set_value(secret.trim());
        auth_input.focus();

        let client = Client::new(base_url, secret);
        let mut app = Self {
            dashboard: Dashboard::new(Arc::clone(&client), locale.clone()),
            config: ConfigTab::new(Arc::clone(&client), locale.clone()),
            auth: AuthTab::new(Arc::clone(&client), locale.clone()),
            keys: KeysTab::new(Arc::clone(&client), locale.clone(), platform.clone()),
            oauth: OAuthTab::new(Some(Arc::clone(&client)), locale.clone(), platform),
            logs: LogsTab::new(Arc::clone(&client), hook, locale.clone()),
            locale,
            active_tab: TAB_DASHBOARD,
            tabs: Vec::new(),
            standalone,
            logs_enabled: true,
            authenticated: !auth_required,
            auth_input,
            auth_error: String::new(),
            auth_connecting: false,
            client,
            width: 0,
            height: 0,
            ready: false,
            initialized: [true, false, false, false, false, true],
        };
        app.refresh_tabs();
        if auth_required {
            app.initialized = [false; 6];
        }
        app.set_auth_input_prompt();
        app
    }

    /// `Init`.
    pub(crate) fn init(&self) -> Option<Cmd> {
        if !self.authenticated {
            return None;
        }
        batch([
            self.dashboard.init(),
            if self.logs_enabled {
                self.logs.init()
            } else {
                None
            },
        ])
    }

    /// `Update`.
    pub(crate) fn update(&mut self, msg: Msg) -> Option<Cmd> {
        match msg {
            Msg::Resize { width, height } => {
                self.width = width;
                self.height = height;
                self.ready = true;
                if width > 0 {
                    self.auth_input.width = width - 6;
                }
                // Less the tab bar and status bar.
                let content_height = (height - 4).max(1);
                self.dashboard.set_size(width, content_height);
                self.config.set_size(width, content_height);
                self.auth.set_size(width, content_height);
                self.keys.set_size(width, content_height);
                self.oauth.set_size(width, content_height);
                self.logs.set_size(width, content_height);
                None
            }
            Msg::AuthConnect(result) => {
                self.auth_connecting = false;
                let cfg = match result {
                    Err(err) => {
                        self.auth_error = fill(self.locale.t("auth_gate_connect_fail"), &[&err]);
                        return None;
                    }
                    Ok(cfg) => cfg,
                };
                self.auth_error.clear();
                self.authenticated = true;
                self.logs_enabled = self.standalone || logs_enabled_from_config(cfg.as_ref());
                self.refresh_tabs();
                self.initialized = [false; 6];
                self.initialized[TAB_DASHBOARD] = true;
                let mut cmd_logs = None;
                if self.logs_enabled {
                    self.initialized[TAB_LOGS] = true;
                    cmd_logs = self.logs.init();
                }
                batch([self.dashboard.init(), cmd_logs])
            }
            Msg::ConfigUpdate(update) => {
                let mut cmd_logs = None;
                if !self.standalone
                    && update.err.is_none()
                    && update.path == "logging-to-file"
                    && let Some(Value::Bool(enabled)) = update.value
                {
                    let before = self.logs_enabled;
                    self.logs_enabled = enabled;
                    if before != enabled {
                        self.refresh_tabs();
                    }
                    if !enabled {
                        self.initialized[TAB_LOGS] = false;
                    }
                    if !before && enabled {
                        self.initialized[TAB_LOGS] = true;
                        cmd_logs = self.logs.init();
                    }
                }
                let cmd_config = self.config.update(Msg::ConfigUpdate(update));
                batch([cmd_config, cmd_logs])
            }
            Msg::Key(key) => self.on_key(key),
            msg => {
                if !self.authenticated {
                    return None;
                }
                self.route(msg)
            }
        }
    }

    fn on_key(&mut self, key: Key) -> Option<Cmd> {
        let name = key.string();
        if !self.authenticated {
            match name.as_str() {
                "ctrl+c" | "q" => return Some(Cmd::Quit),
                "L" => {
                    self.locale.toggle();
                    self.refresh_tabs();
                    self.set_auth_input_prompt();
                }
                "enter" => {
                    if self.auth_connecting {
                        return None;
                    }
                    let password = self.auth_input.value().trim().to_owned();
                    if password.is_empty() {
                        self.auth_error = self.locale.t("auth_gate_password_required").to_owned();
                        return None;
                    }
                    self.auth_error.clear();
                    self.auth_connecting = true;
                    return Some(self.connect_with_password(password));
                }
                _ => self.auth_input.update(&key),
            }
            return None;
        }

        match name.as_str() {
            "ctrl+c" => return Some(Cmd::Quit),
            // In the logs tab, `q` goes to the tab.
            "q" if !self.logs_enabled || self.active_tab != TAB_LOGS => return Some(Cmd::Quit),
            "L" => {
                self.locale.toggle();
                self.refresh_tabs();
                return self.broadcast_locale_changed();
            }
            "tab" | "shift+tab" => {
                let count = self.tabs.len();
                if count == 0 {
                    return None;
                }
                self.active_tab = if name == "tab" {
                    (self.active_tab + 1) % count
                } else {
                    (self.active_tab + count - 1) % count
                };
                return self.init_tab_if_needed();
            }
            _ => {}
        }
        self.route(Msg::Key(key))
    }

    /// Hands `msg` to the active tab; the logs tab also gets its own
    /// messages while another tab is active, so it keeps polling.
    fn route(&mut self, msg: Msg) -> Option<Cmd> {
        let for_logs = matches!(msg, Msg::LogsPoll(_) | Msg::LogsTick | Msg::LogLine(_));
        if for_logs && self.active_tab != TAB_LOGS {
            // The other tabs ignore these.
            return if self.logs_enabled {
                self.logs.update(msg)
            } else {
                None
            };
        }
        match self.active_tab {
            TAB_DASHBOARD => self.dashboard.update(msg),
            TAB_CONFIG => self.config.update(msg),
            TAB_AUTH_FILES => self.auth.update(msg),
            TAB_API_KEYS => self.keys.update(msg),
            TAB_OAUTH => self.oauth.update(msg),
            TAB_LOGS => self.logs.update(msg),
            _ => None,
        }
    }

    /// `broadcastToAllTabs` with `localeChangedMsg`.
    fn broadcast_locale_changed(&mut self) -> Option<Cmd> {
        batch([
            self.dashboard.update(Msg::LocaleChanged),
            self.config.update(Msg::LocaleChanged),
            self.auth.update(Msg::LocaleChanged),
            self.keys.update(Msg::LocaleChanged),
            self.oauth.update(Msg::LocaleChanged),
            self.logs.update(Msg::LocaleChanged),
        ])
    }

    /// `refreshTabs`: the tab names, without the logs tab when it's off.
    fn refresh_tabs(&mut self) {
        let names = self.locale.tab_names();
        self.tabs = names
            .iter()
            .enumerate()
            .filter(|&(idx, _)| self.logs_enabled || idx != TAB_LOGS)
            .map(|(_, name)| *name)
            .collect();
        if self.tabs.is_empty() {
            self.active_tab = TAB_DASHBOARD;
            return;
        }
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        }
    }

    /// `initTabIfNeeded`: fetches the active tab's data the first time it
    /// is shown.
    fn init_tab_if_needed(&mut self) -> Option<Cmd> {
        let initialized = self.initialized.get_mut(self.active_tab)?;
        if *initialized {
            return None;
        }
        *initialized = true;
        match self.active_tab {
            TAB_DASHBOARD => self.dashboard.init(),
            TAB_CONFIG => self.config.init(),
            TAB_AUTH_FILES => self.auth.init(),
            TAB_API_KEYS => self.keys.init(),
            TAB_OAUTH => self.oauth.init(),
            TAB_LOGS if self.logs_enabled => self.logs.init(),
            _ => None,
        }
    }

    /// `View`.
    pub(crate) fn view(&self) -> String {
        if !self.authenticated {
            return self.render_auth_view();
        }
        if !self.ready {
            return self.locale.t("initializing_tui").to_owned();
        }
        let mut sb = self.render_tab_bar();
        sb.push('\n');
        match self.active_tab {
            TAB_DASHBOARD => sb.push_str(&self.dashboard.view()),
            TAB_CONFIG => sb.push_str(&self.config.view()),
            TAB_AUTH_FILES => sb.push_str(&self.auth.view()),
            TAB_API_KEYS => sb.push_str(&self.keys.view()),
            TAB_OAUTH => sb.push_str(&self.oauth.view()),
            TAB_LOGS if self.logs_enabled => sb.push_str(&self.logs.view()),
            _ => {}
        }
        sb.push('\n');
        sb.push_str(&self.render_status_bar());
        sb
    }

    /// `renderAuthView`: the key gate.
    fn render_auth_view(&self) -> String {
        let t = |key: &'static str| self.locale.t(key);
        let mut sb = styles::TITLE.render(t("auth_gate_title"));
        sb.push('\n');
        sb.push_str(&styles::HELP.render(t("auth_gate_help")));
        sb.push_str("\n\n");
        if self.auth_connecting {
            sb.push_str(&styles::WARNING.render(t("auth_gate_connecting")));
            sb.push_str("\n\n");
        }
        if !self.auth_error.trim().is_empty() {
            sb.push_str(&styles::ERROR.render(&self.auth_error));
            sb.push_str("\n\n");
        }
        sb.push_str(&self.auth_input.view());
        sb.push('\n');
        sb.push_str(&styles::HELP.render(t("auth_gate_enter")));
        sb
    }

    /// `renderTabBar`.
    fn render_tab_bar(&self) -> String {
        let tabs: Vec<String> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, name)| {
                if i == self.active_tab {
                    styles::TAB_ACTIVE.render(name)
                } else {
                    styles::TAB_INACTIVE.render(name)
                }
            })
            .collect();
        let tabs: Vec<&str> = tabs.iter().map(String::as_str).collect();
        styles::TAB_BAR
            .width(self.width)
            .render(&crate::style::join_horizontal(&tabs))
    }

    /// `renderStatusBar`: the key help, left and right, cut to fit.
    fn render_status_bar(&self) -> String {
        let mut left = self
            .locale
            .t("status_left")
            .trim_end_matches(' ')
            .to_owned();
        let mut right = self
            .locale
            .t("status_right")
            .trim_end_matches(' ')
            .to_owned();

        let width = self.width.max(1);
        // The bar has a cell of padding on each side.
        let content_width = usize::try_from(width - 2).unwrap_or(0);

        if crate::ansi::block_width(&left) > content_width {
            left = fit_string_width(&left, content_width);
            right.clear();
        }
        let remaining = content_width.saturating_sub(crate::ansi::block_width(&left));
        if crate::ansi::block_width(&right) > remaining {
            right = fit_string_width(&right, remaining);
        }
        let gap = content_width
            .saturating_sub(crate::ansi::block_width(&left))
            .saturating_sub(crate::ansi::block_width(&right));
        styles::STATUS_BAR
            .width(width)
            .render(&format!("{left}{}{right}", " ".repeat(gap)))
    }

    /// `setAuthInputPrompt`.
    fn set_auth_input_prompt(&mut self) {
        self.auth_input.prompt = format!("  {}: ", self.locale.t("auth_gate_password"));
    }

    /// `connectWithPassword`: switches the client to `password` and checks
    /// it by fetching the config.
    fn connect_with_password(&self, password: String) -> Cmd {
        let client = Arc::clone(&self.client);
        Cmd::run(async move {
            client.set_secret_key(&password);
            Some(Msg::AuthConnect(client.get_config().await))
        })
    }
}

/// `fitStringWidth`: the longest start of `text` at most `max_width` cells
/// wide.
fn fit_string_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if crate::ansi::block_width(text) <= max_width {
        return text.to_owned();
    }
    let mut out = String::new();
    for c in text.chars() {
        let mut next = out.clone();
        next.push(c);
        if crate::ansi::block_width(&next) > max_width {
            break;
        }
        out = next;
    }
    out
}

/// `isLogsEnabledFromConfig`: `logging-to-file`, when it is a bool.
fn logs_enabled_from_config(cfg: Option<&Object>) -> bool {
    match cfg.and_then(|cfg| cfg.get("logging-to-file")) {
        Some(Value::Bool(enabled)) => *enabled,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ports TestNewAppWithBaseURL.
    #[test]
    fn new_app_with_base_url() {
        let app = App::new(
            "https://proxy.example.com",
            "secret",
            None,
            Platform::none(),
        );
        assert_eq!(app.client.base_url(), "https://proxy.example.com");
    }

    // Not upstream's: the logs tab shows unless the config turns file
    // logging off.
    #[test]
    fn reads_logs_enabled_as_upstream_does() {
        let cfg = |v: Value| {
            let mut m = Object::new();
            m.insert("logging-to-file".to_owned(), v);
            m
        };
        assert!(logs_enabled_from_config(None));
        assert!(logs_enabled_from_config(Some(&Object::new())));
        assert!(logs_enabled_from_config(Some(&cfg(Value::from("no")))));
        assert!(logs_enabled_from_config(Some(&cfg(Value::Bool(true)))));
        assert!(!logs_enabled_from_config(Some(&cfg(Value::Bool(false)))));
    }

    // Not upstream's: text is cut to whole characters that fit.
    #[test]
    fn fits_string_width_as_upstream_does() {
        assert_eq!(fit_string_width("abc", 0), "");
        assert_eq!(fit_string_width("abc", 5), "abc");
        assert_eq!(fit_string_width("abcdef", 4), "abcd");
        assert_eq!(fit_string_width("日本語", 5), "日本");
    }
}
