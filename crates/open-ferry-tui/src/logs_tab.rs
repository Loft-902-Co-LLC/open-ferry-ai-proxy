// Ported from CLIProxyAPI internal/tui/logs_tab.go (newLogsTabModel, Init,
// fetchLogs, waitForNextPoll, waitForLog, Update, SetSize, View,
// renderLogs, matchLevel, styleLine) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The logs tab: the server's log lines as they come, filtered by level.
//! In standalone mode they come from the embedded server's [`LogHook`];
//! otherwise the tab polls the management API for them every 2 seconds.
//!
//! Deviations from upstream:
//! - The rule under the help line is empty at a negative width, where
//!   upstream's panics.

use std::sync::Arc;
use std::time::Duration;

use crate::app::Msg;
use crate::client::Client;
use crate::i18n::Locale;
use crate::loghook::LogHook;
use crate::styles;
use crate::tea::Cmd;
use crate::viewport::Viewport;

/// How many lines the tab keeps.
const MAX_LINES: usize = 5000;

/// How long the tab waits between polls.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The logs tab (upstream's `logsTabModel`).
pub(crate) struct LogsTab {
    client: Arc<Client>,
    hook: Option<LogHook>,
    locale: Locale,
    viewport: Viewport,
    lines: Vec<String>,
    auto_scroll: bool,
    width: i64,
    ready: bool,
    /// `""`, `"info"`, `"warn"` or `"error"`.
    filter: &'static str,
    after: i64,
    last_err: Option<String>,
}

impl LogsTab {
    /// `newLogsTabModel`.
    pub(crate) fn new(client: Arc<Client>, hook: Option<LogHook>, locale: Locale) -> Self {
        Self {
            client,
            hook,
            locale,
            viewport: Viewport::default(),
            lines: Vec::new(),
            auto_scroll: true,
            width: 0,
            ready: false,
            filter: "",
            after: 0,
            last_err: None,
        }
    }

    /// `Init`.
    pub(crate) fn init(&self) -> Option<Cmd> {
        Some(match &self.hook {
            Some(hook) => wait_for_log(hook.clone()),
            None => self.fetch_logs(),
        })
    }

    /// `fetchLogs`: the lines after the last poll's.
    fn fetch_logs(&self) -> Cmd {
        let client = Arc::clone(&self.client);
        let after = self.after;
        Cmd::run(async move { Some(Msg::LogsPoll(client.get_logs(after, 200).await)) })
    }

    /// `Update`.
    pub(crate) fn update(&mut self, msg: Msg) -> Option<Cmd> {
        match msg {
            Msg::LocaleChanged => {
                self.refresh();
                None
            }
            Msg::LogsTick => {
                if self.hook.is_some() {
                    return None;
                }
                Some(self.fetch_logs())
            }
            Msg::LogsPoll(result) => {
                if self.hook.is_some() {
                    return None;
                }
                match result {
                    Err(err) => self.last_err = Some(err),
                    Ok((lines, latest)) => {
                        self.last_err = None;
                        self.after = latest;
                        self.append(lines);
                    }
                }
                self.refresh();
                if self.auto_scroll {
                    self.viewport.goto_bottom();
                }
                // `waitForNextPoll`.
                Some(Cmd::Tick(POLL_INTERVAL, Box::new(Msg::LogsTick)))
            }
            Msg::LogLine(line) => {
                self.append(vec![line]);
                self.refresh();
                if self.auto_scroll {
                    self.viewport.goto_bottom();
                }
                self.hook.clone().map(wait_for_log)
            }
            Msg::Key(key) => {
                match key.string().as_str() {
                    "a" => {
                        self.auto_scroll = !self.auto_scroll;
                        if self.auto_scroll {
                            self.viewport.goto_bottom();
                        }
                    }
                    "c" => {
                        self.lines.clear();
                        self.last_err = None;
                        self.refresh();
                    }
                    "1" => self.set_filter(""),
                    "2" => self.set_filter("info"),
                    "3" => self.set_filter("warn"),
                    "4" => self.set_filter("error"),
                    _ => {
                        let was_at_bottom = self.viewport.at_bottom();
                        self.viewport.update(&key);
                        // Scrolling up pauses; scrolling to the bottom
                        // resumes.
                        if !self.viewport.at_bottom() && was_at_bottom {
                            self.auto_scroll = false;
                        }
                        if self.viewport.at_bottom() {
                            self.auto_scroll = true;
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn append(&mut self, lines: Vec<String>) {
        self.lines.extend(lines);
        if self.lines.len() > MAX_LINES {
            let excess = self.lines.len() - MAX_LINES;
            self.lines.drain(..excess);
        }
    }

    fn set_filter(&mut self, filter: &'static str) {
        self.filter = filter;
        self.refresh();
    }

    fn refresh(&mut self) {
        let content = self.render_logs();
        self.viewport.set_content(&content);
    }

    /// `SetSize`.
    pub(crate) fn set_size(&mut self, width: i64, height: i64) {
        self.width = width;
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

    /// `renderLogs`.
    fn render_logs(&self) -> String {
        let t = |key: &'static str| self.locale.t(key);
        let mut sb = String::new();

        let scroll_status = if self.auto_scroll {
            styles::SUCCESS.render(t("logs_auto_scroll"))
        } else {
            styles::WARNING.render(t("logs_paused"))
        };
        let filter_label = if self.filter.is_empty() {
            "ALL".to_owned()
        } else {
            format!("{}+", self.filter.to_uppercase())
        };
        let header = format!(
            " {}  {scroll_status}  {}: {filter_label}  {}: {}",
            t("logs_title"),
            t("logs_filter"),
            t("logs_lines"),
            self.lines.len()
        );
        sb.push_str(&styles::TITLE.render(&header));
        sb.push('\n');
        sb.push_str(&styles::HELP.render(t("logs_help")));
        sb.push('\n');
        sb.push_str(&styles::rule(self.width));
        sb.push('\n');

        if let Some(err) = &self.last_err {
            sb.push_str(&styles::ERROR.render(&format!("⚠ Error: {err}")));
            sb.push('\n');
        }

        if self.lines.is_empty() {
            sb.push_str(&styles::SUBTITLE.render(t("logs_waiting")));
            return sb;
        }

        for line in &self.lines {
            if !self.filter.is_empty() && !self.match_level(line) {
                continue;
            }
            sb.push_str(&style_line(line));
            sb.push('\n');
        }
        sb
    }

    /// `matchLevel`: whether `line` is at the filter's level or above.
    fn match_level(&self, line: &str) -> bool {
        match self.filter {
            "error" => {
                line.contains("[error]") || line.contains("[fatal]") || line.contains("[panic]")
            }
            "warn" => {
                line.contains("[warn") || line.contains("[error]") || line.contains("[fatal]")
            }
            "info" => !line.contains("[debug]"),
            _ => true,
        }
    }
}

/// `waitForLog`: the hook's next line.
fn wait_for_log(hook: LogHook) -> Cmd {
    Cmd::run(async move { Some(Msg::LogLine(hook.recv().await)) })
}

/// `styleLine`: `line` in its level's colour.
fn style_line(line: &str) -> String {
    if line.contains("[error]") || line.contains("[fatal]") {
        styles::LOG_ERROR.render(line)
    } else if line.contains("[warn") {
        styles::LOG_WARN.render(line)
    } else if line.contains("[info") {
        styles::LOG_INFO.render(line)
    } else if line.contains("[debug]") {
        styles::LOG_DEBUG.render(line)
    } else {
        line.to_owned()
    }
}
