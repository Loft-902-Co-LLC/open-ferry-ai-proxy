// Ported from CLIProxyAPI internal/tui/dashboard.go (newDashboardModel, Init,
// fetchData, Update, SetSize, View, renderDashboard, getFloat, getBool,
// boolEmoji, minInt) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The dashboard tab: the server's address, the number of management keys
//! and auth files, and the main config settings.
//!
//! Upstream's `formatKV`, `formatLargeNumber` and `truncate` aren't ported:
//! nothing calls them.
//!
//! Deviations from upstream: none.

use std::sync::Arc;

use serde_json::Value;

use crate::app::Msg;
use crate::client::{Client, Object, get_string};
use crate::i18n::Locale;
use crate::style::{Color, ROUNDED_BORDER, Style, join_horizontal};
use crate::styles;
use crate::tea::Cmd;
use crate::viewport::Viewport;

/// What [`Dashboard`] fetched (upstream's `dashboardDataMsg`).
#[derive(Debug, Clone)]
pub(crate) struct Data {
    pub(crate) config: Option<Object>,
    pub(crate) auth_files: Vec<Object>,
    pub(crate) api_keys: Vec<String>,
    pub(crate) err: Option<String>,
}

/// The dashboard tab (upstream's `dashboardModel`).
pub(crate) struct Dashboard {
    client: Arc<Client>,
    locale: Locale,
    viewport: Viewport,
    content: String,
    width: i64,
    ready: bool,
    last_config: Option<Object>,
    last_auth_files: Vec<Object>,
    last_api_keys: Vec<String>,
}

impl Dashboard {
    /// `newDashboardModel`.
    pub(crate) fn new(client: Arc<Client>, locale: Locale) -> Self {
        Self {
            client,
            locale,
            viewport: Viewport::default(),
            content: String::new(),
            width: 0,
            ready: false,
            last_config: None,
            last_auth_files: Vec::new(),
            last_api_keys: Vec::new(),
        }
    }

    /// `Init`.
    pub(crate) fn init(&self) -> Option<Cmd> {
        Some(self.fetch_data())
    }

    /// `fetchData`: the config, auth files and keys; the first error wins.
    fn fetch_data(&self) -> Cmd {
        let client = Arc::clone(&self.client);
        Cmd::run(async move {
            let config = client.get_config().await;
            let auth_files = client.get_auth_files().await;
            let api_keys = client.get_api_keys().await;
            let err = [
                config.as_ref().err(),
                auth_files.as_ref().err(),
                api_keys.as_ref().err(),
            ]
            .into_iter()
            .flatten()
            .next()
            .cloned();
            Some(Msg::Dashboard(Data {
                config: config.ok().flatten(),
                auth_files: auth_files.unwrap_or_default(),
                api_keys: api_keys.unwrap_or_default(),
                err,
            }))
        })
    }

    /// `Update`.
    pub(crate) fn update(&mut self, msg: Msg) -> Option<Cmd> {
        match msg {
            Msg::LocaleChanged => {
                self.content = self.render(
                    self.last_config.as_ref(),
                    &self.last_auth_files,
                    &self.last_api_keys,
                );
                self.viewport.set_content(&self.content);
                Some(self.fetch_data())
            }
            Msg::Dashboard(data) => {
                if let Some(err) = data.err {
                    self.content = styles::ERROR.render(&format!("⚠ Error: {err}"));
                } else {
                    self.content =
                        self.render(data.config.as_ref(), &data.auth_files, &data.api_keys);
                    self.last_config = data.config;
                    self.last_auth_files = data.auth_files;
                    self.last_api_keys = data.api_keys;
                }
                self.viewport.set_content(&self.content);
                None
            }
            Msg::Key(key) => {
                if key.string() == "r" {
                    return Some(self.fetch_data());
                }
                self.viewport.update(&key);
                None
            }
            _ => None,
        }
    }

    /// `SetSize`.
    pub(crate) fn set_size(&mut self, width: i64, height: i64) {
        self.width = width;
        if self.ready {
            self.viewport.width = width;
            self.viewport.height = height;
        } else {
            self.viewport = Viewport::new(width, height);
            self.viewport.set_content(&self.content);
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

    /// `renderDashboard`.
    fn render(&self, cfg: Option<&Object>, auth_files: &[Object], api_keys: &[String]) -> String {
        let t = |key| self.locale.t(key);
        let mut sb = String::new();
        sb.push_str(&styles::TITLE.render(t("dashboard_title")));
        sb.push('\n');
        sb.push_str(&styles::HELP.render(t("dashboard_help")));
        sb.push_str("\n\n");

        let conn = Style::new().bold(true).fg(styles::COLOR_SUCCESS);
        sb.push_str(&conn.render(t("connected")));
        sb.push_str("  ");
        sb.push_str(self.client.base_url());
        sb.push_str("\n\n");

        let card_width = if self.width > 0 {
            ((self.width - 2) / 2).max(18)
        } else {
            25
        };
        let card = Style::new()
            .border(ROUNDED_BORDER)
            .border_fg(Color::Ansi(240))
            .padding(0, 1)
            .width(card_width)
            .height(2);
        let card1 = card.render(&format!(
            "{}\n{}",
            Style::new()
                .bold(true)
                .fg(Color::Ansi(111))
                .render(&format!("🔑 {}", api_keys.len())),
            Style::new().fg(styles::COLOR_MUTED).render(t("mgmt_keys")),
        ));
        let active = auth_files
            .iter()
            .filter(|f| !get_bool(f, "disabled"))
            .count();
        let card2 = card.render(&format!(
            "{}\n{}",
            Style::new()
                .bold(true)
                .fg(Color::Ansi(76))
                .render(&format!("📄 {}", auth_files.len())),
            Style::new().fg(styles::COLOR_MUTED).render(&format!(
                "{} ({active} {})",
                t("auth_files_label"),
                t("active_suffix")
            )),
        ));
        sb.push_str(&join_horizontal(&[&card1, " ", &card2]));
        sb.push_str("\n\n");

        sb.push_str(
            &Style::new()
                .bold(true)
                .fg(styles::COLOR_HIGHLIGHT)
                .render(t("current_config")),
        );
        sb.push('\n');
        sb.push_str(&styles::rule(self.width.min(60)));
        sb.push('\n');

        if let Some(cfg) = cfg {
            let usage_enabled = match cfg.get("usage-statistics-enabled") {
                Some(Value::Bool(b)) => *b,
                _ => true,
            };
            let mut items = vec![
                (t("debug_mode"), self.bool_emoji(get_bool(cfg, "debug"))),
                (t("usage_stats"), self.bool_emoji(usage_enabled)),
                (
                    t("log_to_file"),
                    self.bool_emoji(get_bool(cfg, "logging-to-file")),
                ),
                (
                    t("retry_count"),
                    format!("{:.0}", get_float(cfg, "request-retry")),
                ),
            ];
            let proxy_url = get_string(cfg, "proxy-url");
            if !proxy_url.is_empty() {
                items.push((t("proxy_url"), proxy_url));
            }
            for (label, value) in items {
                sb.push_str(&format_kv(label, &value));
            }
            let mut strategy = "round-robin".to_owned();
            if let Some(Value::Object(routing)) = cfg.get("routing") {
                let s = get_string(routing, "strategy");
                if !s.is_empty() {
                    strategy = s;
                }
            }
            sb.push_str(&format_kv(t("routing_strategy"), &strategy));
        }

        sb.push('\n');
        sb
    }

    /// `boolEmoji`.
    fn bool_emoji(&self, b: bool) -> String {
        self.locale
            .t(if b { "bool_yes" } else { "bool_no" })
            .to_owned()
    }
}

/// A labelled config line, as `renderDashboard` writes each.
fn format_kv(label: &str, value: &str) -> String {
    format!(
        "  {} {}\n",
        styles::LABEL.render(&format!("{label}:")),
        styles::VALUE.render(value)
    )
}

/// `getFloat`: the number under `key`, else 0.
pub(crate) fn get_float(m: &Object, key: &str) -> f64 {
    match m.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// `getBool`: the bool under `key`, else false.
pub(crate) fn get_bool(m: &Object, key: &str) -> bool {
    matches!(m.get(key), Some(Value::Bool(true)))
}
