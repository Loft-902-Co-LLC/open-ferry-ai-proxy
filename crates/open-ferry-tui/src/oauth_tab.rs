// Ported from CLIProxyAPI internal/tui/oauth_tab.go (oauthProviders,
// newOAuthTabModel, Init, Update, startOAuth, cancelRemoteOAuth,
// cancelOAuthSession, submitCallback, pollOAuthStatus,
// shouldAcceptOAuthStart, shouldAcceptOAuthPoll, shouldFailOAuthStatusPoll,
// SetSize, View, renderContent, renderRemoteMode, renderDeviceMode,
// wrapText) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The OAuth tab: signs the server in to a provider. It asks the server for
//! the provider's sign-in URL, opens it in a browser, and polls the server
//! until the sign-in ends; for a browser sign-in, the URL the browser was
//! sent back to can be pasted in, for a server that can't receive it
//! itself.
//!
//! Only Claude and Codex are offered. Upstream also offers Antigravity,
//! Kimi (both sites), xAI and Meta, which open-ferry doesn't sign in to.
//!
//! As upstream's does, the callback URL input's prompt is in Chinese in
//! both locales.
//!
//! Deviations from upstream:
//! - A poll stops once its sign-in is cancelled or another starts.
//!   Upstream's keeps asking the server for the old sign-in's status until
//!   it ends or times out, and then drops the answer.
//! - A sign-in the server says expires in over a day is polled for a day.
//!   Upstream's timeout can overflow and panic.
//! - The sign-in URL wraps by characters. Upstream wraps by bytes, which
//!   can split a character; the two agree on ASCII.
//! - With no client, as upstream's tests make the tab, starting a sign-in,
//!   polling and submitting a callback URL do nothing. Upstream's panic.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::json;
use tokio::time::Instant;

use crate::app::{Msg, Platform};
use crate::client::{Client, get_string};
use crate::dashboard::get_float;
use crate::i18n::{Locale, fill};
use crate::keys::Key;
use crate::style::{Color, Style};
use crate::styles;
use crate::tea::Cmd;
use crate::textinput::TextInput;
use crate::viewport::Viewport;

/// A provider the tab can sign in to (upstream's `oauthProvider`).
#[derive(Debug, Clone, Copy)]
struct Provider {
    name: &'static str,
    /// The management route that starts the sign-in.
    api_path: &'static str,
    emoji: &'static str,
    /// Whether the provider signs in with a device code (RFC 8628).
    device_flow: bool,
    /// The provider's name in an `oauth-callback` request.
    key: &'static str,
}

/// `oauthProviders`, without those open-ferry doesn't sign in to.
const PROVIDERS: [Provider; 2] = [
    Provider {
        name: "Claude (Anthropic)",
        api_path: "anthropic-auth-url",
        emoji: "🟧",
        device_flow: false,
        key: "anthropic",
    },
    Provider {
        name: "Codex (OpenAI)",
        api_path: "codex-auth-url",
        emoji: "🟩",
        device_flow: false,
        key: "codex",
    },
];

/// `defaultOAuthPollTimeout`.
const DEFAULT_POLL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// `deviceOAuthPollTimeout`.
const DEVICE_POLL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The longest a poll runs, whatever the server says.
const MAX_POLL_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// `maxOAuthStatusPollErrors`.
const MAX_POLL_ERRORS: i64 = 5;
/// `oauthStatusPollInterval`.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Where a sign-in is (upstream's `oauthState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Idle,
    Pending,
    /// Waiting for the browser or device sign-in.
    Remote,
    Success,
    Error,
}

/// A started sign-in, or why it didn't start (upstream's `oauthStartMsg`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Start {
    url: String,
    state: String,
    provider_name: String,
    user_code: String,
    device_flow: bool,
    expires_in: i64,
    generation: u64,
    err: Option<String>,
}

/// A poll's result (upstream's `oauthPollMsg`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Poll {
    state: String,
    generation: u64,
    done: bool,
    message: String,
    err: Option<String>,
}

/// The OAuth tab (upstream's `oauthTabModel`).
pub(crate) struct OAuthTab {
    client: Option<Arc<Client>>,
    locale: Locale,
    platform: Platform,
    viewport: Viewport,
    cursor: usize,
    state: State,
    message: String,
    err: Option<String>,
    width: i64,
    ready: bool,
    auth_url: String,
    auth_state: String,
    provider_name: String,
    user_code: String,
    device_flow: bool,
    expires_in: i64,
    callback_input: TextInput,
    input_active: bool,
    poll_generation: u64,
    /// `poll_generation`, for running polls to see.
    live_generation: Arc<AtomicU64>,
}

impl OAuthTab {
    /// `newOAuthTabModel`.
    pub(crate) fn new(client: Option<Arc<Client>>, locale: Locale, platform: Platform) -> Self {
        let mut callback_input = TextInput::new();
        callback_input.placeholder =
            "http://localhost:.../auth/callback?code=...&state=...".to_owned();
        callback_input.char_limit = 2048;
        callback_input.prompt = "  回调 URL: ".to_owned();
        Self {
            client,
            locale,
            platform,
            viewport: Viewport::default(),
            cursor: 0,
            state: State::Idle,
            message: String::new(),
            err: None,
            width: 0,
            ready: false,
            auth_url: String::new(),
            auth_state: String::new(),
            provider_name: String::new(),
            user_code: String::new(),
            device_flow: false,
            expires_in: 0,
            callback_input,
            input_active: false,
            poll_generation: 0,
            live_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// `Init`.
    pub(crate) fn init(&self) -> Option<Cmd> {
        None
    }

    fn refresh(&mut self) {
        let content = self.render_content();
        self.viewport.set_content(&content);
    }

    fn next_generation(&mut self) {
        self.poll_generation = self.poll_generation.wrapping_add(1);
        self.live_generation
            .store(self.poll_generation, Ordering::Relaxed);
    }

    /// `Update`.
    pub(crate) fn update(&mut self, msg: Msg) -> Option<Cmd> {
        match msg {
            Msg::LocaleChanged => {
                self.refresh();
                None
            }
            Msg::OAuthStart(start) => self.on_start(start),
            Msg::OAuthPoll(poll) => {
                if !should_accept_poll(&poll, &self.auth_state, self.poll_generation, self.state) {
                    return None;
                }
                if let Some(err) = poll.err {
                    self.state = State::Error;
                    self.message = styles::ERROR.render(&format!("✗ {err}"));
                    self.err = Some(err);
                    self.input_active = false;
                    self.callback_input.blur();
                } else if poll.done {
                    self.state = State::Success;
                    self.message = styles::SUCCESS.render(&format!("✓ {}", poll.message));
                    self.input_active = false;
                    self.callback_input.blur();
                } else {
                    self.message = styles::WARNING.render(&format!("⏳ {}", poll.message));
                }
                self.refresh();
                None
            }
            Msg::OAuthCallbackSubmit(result) => {
                self.message = match result {
                    Err(err) => styles::ERROR
                        .render(&format!("{}: {err}", self.locale.t("oauth_submit_fail"))),
                    Ok(()) => styles::SUCCESS.render(self.locale.t("oauth_submit_ok")),
                };
                self.refresh();
                None
            }
            Msg::Key(key) => self.on_key(&key),
            _ => None,
        }
    }

    fn on_start(&mut self, start: Start) -> Option<Cmd> {
        if !should_accept_start(&start, self.poll_generation) {
            // A sign-in started before the user cancelled it: cancel it on
            // the server, so its credentials aren't saved.
            if start.err.is_none() && !start.state.trim().is_empty() {
                return self.cancel_session(&start.state);
            }
            return None;
        }
        if let Some(err) = start.err {
            self.state = State::Error;
            self.message = styles::ERROR.render(&format!("✗ {err}"));
            self.err = Some(err);
            self.refresh();
            return None;
        }
        self.auth_url = start.url;
        self.auth_state = start.state;
        self.provider_name = start.provider_name;
        self.user_code = start.user_code;
        self.device_flow = start.device_flow;
        self.expires_in = start.expires_in;
        self.state = State::Remote;
        self.callback_input.set_value("");
        self.message.clear();
        if self.device_flow {
            self.input_active = false;
            self.callback_input.blur();
        } else {
            self.callback_input.focus();
            self.input_active = true;
        }
        self.refresh();
        self.poll_status(start.generation)
    }

    fn on_key(&mut self, key: &Key) -> Option<Cmd> {
        let name = key.string();

        // Typing the callback URL (browser sign-ins only).
        if self.input_active && !self.device_flow {
            return match name.as_str() {
                "enter" => {
                    let callback_url = self.callback_input.value();
                    if callback_url.is_empty() {
                        return None;
                    }
                    self.input_active = false;
                    self.callback_input.blur();
                    self.message = styles::WARNING.render(self.locale.t("oauth_submitting"));
                    self.refresh();
                    self.submit_callback(callback_url)
                }
                "esc" => self.cancel_remote(),
                _ => {
                    self.callback_input.update(key);
                    self.refresh();
                    None
                }
            };
        }

        match self.state {
            State::Remote => match name.as_str() {
                "c" | "C" => {
                    if self.device_flow {
                        return None;
                    }
                    self.input_active = true;
                    self.callback_input.focus();
                    self.refresh();
                    None
                }
                "esc" => self.cancel_remote(),
                _ => {
                    self.viewport.update(key);
                    None
                }
            },
            State::Pending => {
                if name == "esc" {
                    self.next_generation();
                    self.state = State::Idle;
                    self.message.clear();
                    self.refresh();
                }
                None
            }
            State::Idle | State::Success | State::Error => match name.as_str() {
                "up" | "k" => {
                    if self.cursor > 0 {
                        self.cursor -= 1;
                        self.refresh();
                    }
                    None
                }
                "down" | "j" => {
                    if self.cursor + 1 < PROVIDERS.len() {
                        self.cursor += 1;
                        self.refresh();
                    }
                    None
                }
                "enter" => {
                    let provider = *PROVIDERS.get(self.cursor)?;
                    self.next_generation();
                    self.state = State::Pending;
                    self.message = styles::WARNING
                        .render(&fill(self.locale.t("oauth_initiating"), &[provider.name]));
                    self.refresh();
                    self.start_oauth(provider)
                }
                "esc" => {
                    self.state = State::Idle;
                    self.message.clear();
                    self.err = None;
                    self.refresh();
                    None
                }
                _ => {
                    self.viewport.update(key);
                    None
                }
            },
        }
    }

    /// `startOAuth`: asks the server to start the sign-in and opens its
    /// URL.
    fn start_oauth(&self, provider: Provider) -> Option<Cmd> {
        let client = Arc::clone(self.client.as_ref()?);
        let open_url = Arc::clone(&self.platform.open_url);
        let generation = self.poll_generation;
        Some(Cmd::run(async move {
            let path = format!("/v0/management/{}?is_webui=true", provider.api_path);
            let failed = |err: String| {
                Some(Msg::OAuthStart(Start {
                    generation,
                    err: Some(err),
                    ..Start::default()
                }))
            };
            let data = match client.get_json_path(&path).await {
                Ok(data) => data,
                Err(err) => {
                    return failed(format!("failed to start {} login: {err}", provider.name));
                }
            };
            let url = get_string(&data, "url");
            let state = get_string(&data, "state");
            if url.is_empty() {
                return failed(format!("no auth URL returned for {}", provider.name));
            }
            let user_code = get_string(&data, "user_code");
            let flow = get_string(&data, "flow").trim().to_lowercase();
            let expires_in = get_float(&data, "expires_in") as i64;
            let device_flow = provider.device_flow || flow == "device" || !user_code.is_empty();

            // Best effort, as upstream's is.
            open_url(&url);

            Some(Msg::OAuthStart(Start {
                url,
                state,
                provider_name: provider.name.to_owned(),
                user_code,
                device_flow,
                expires_in,
                generation,
                err: None,
            }))
        }))
    }

    /// `cancelRemoteOAuth`: forgets the sign-in and cancels it on the
    /// server.
    fn cancel_remote(&mut self) -> Option<Cmd> {
        let state = std::mem::take(&mut self.auth_state);
        self.next_generation();
        self.state = State::Idle;
        self.message.clear();
        self.auth_url.clear();
        self.user_code.clear();
        self.device_flow = false;
        self.expires_in = 0;
        self.input_active = false;
        self.callback_input.blur();
        self.callback_input.set_value("");
        self.refresh();
        self.cancel_session(&state)
    }

    /// `cancelOAuthSession`.
    fn cancel_session(&self, state: &str) -> Option<Cmd> {
        let state = state.trim().to_owned();
        if state.is_empty() {
            return None;
        }
        let client = Arc::clone(self.client.as_ref()?);
        Some(Cmd::run(async move {
            let _ = client.cancel_auth_session(&state).await;
            None
        }))
    }

    /// `submitCallback`: sends the server the URL the browser was sent back
    /// to.
    fn submit_callback(&self, callback_url: String) -> Option<Cmd> {
        let client = Arc::clone(self.client.as_ref()?);
        let provider = PROVIDERS
            .iter()
            .find(|p| p.name == self.provider_name)
            .map_or("", |p| p.key);
        let body = json!({
            "provider": provider,
            "redirect_url": callback_url,
            "state": self.auth_state,
        });
        Some(Cmd::run(async move {
            let result = client
                .post_json("/v0/management/oauth-callback", &body)
                .await;
            Some(Msg::OAuthCallbackSubmit(result))
        }))
    }

    /// `pollOAuthStatus`: asks the server how the sign-in is going every 2
    /// seconds until it ends, fails five times in a row, times out, or is
    /// no longer the current one.
    fn poll_status(&self, generation: u64) -> Option<Cmd> {
        let client = Arc::clone(self.client.as_ref()?);
        let locale = self.locale.clone();
        let live = Arc::clone(&self.live_generation);
        let state = self.auth_state.clone();
        let timeout = poll_timeout(self.expires_in, self.device_flow);
        Some(Cmd::run(async move {
            let deadline = Instant::now() + timeout;
            let mut consecutive_errors = 0;
            let result = |done: bool, message: &str, err: Option<String>| {
                Some(Msg::OAuthPoll(Poll {
                    state: state.clone(),
                    generation,
                    done,
                    message: message.to_owned(),
                    err,
                }))
            };
            loop {
                if Instant::now() > deadline {
                    return result(false, "", Some(locale.t("oauth_timeout").to_owned()));
                }
                tokio::time::sleep(POLL_INTERVAL).await;
                if live.load(Ordering::Relaxed) != generation {
                    return None;
                }
                let (status, err_msg) = match client.get_auth_status(&state).await {
                    Ok(status) => status,
                    Err(err) => {
                        consecutive_errors += 1;
                        if should_fail_status_poll(consecutive_errors, MAX_POLL_ERRORS) {
                            let err = format!("{}: {err}", locale.t("oauth_status_error"));
                            return result(false, "", Some(err));
                        }
                        continue;
                    }
                };
                consecutive_errors = 0;
                match status.as_str() {
                    "ok" => return result(true, locale.t("oauth_success"), None),
                    "error" => {
                        let err = format!("{}: {err_msg}", locale.t("oauth_failed"));
                        return result(false, "", Some(err));
                    }
                    "wait" => {}
                    _ => return result(true, locale.t("oauth_completed"), None),
                }
            }
        }))
    }

    /// `SetSize`.
    pub(crate) fn set_size(&mut self, width: i64, height: i64) {
        self.width = width;
        self.callback_input.width = width - 16;
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
        let t = |key: &'static str| self.locale.t(key);
        let mut sb = String::new();
        sb.push_str(&styles::TITLE.render(t("oauth_title")));
        sb.push_str("\n\n");

        if !self.message.is_empty() {
            sb.push_str("  ");
            sb.push_str(&self.message);
            sb.push_str("\n\n");
        }

        if self.state == State::Remote {
            if self.device_flow {
                self.render_device_mode(&mut sb);
            } else {
                self.render_remote_mode(&mut sb);
            }
            return sb;
        }

        if self.state == State::Pending {
            sb.push_str(&styles::HELP.render(t("oauth_press_esc")));
            return sb;
        }

        sb.push_str(&styles::HELP.render(t("oauth_select")));
        sb.push_str("\n\n");

        for (i, p) in PROVIDERS.iter().enumerate() {
            let selected = i == self.cursor;
            let prefix = if selected { "▸ " } else { "  " };
            let label = format!("{} {}", p.emoji, p.name);
            let label = if selected {
                Style::new()
                    .bold(true)
                    .fg(styles::COLOR_WHITE)
                    .bg(styles::COLOR_PRIMARY)
                    .padding(0, 1)
                    .render(&label)
            } else {
                Style::new()
                    .fg(styles::COLOR_TEXT)
                    .padding(0, 1)
                    .render(&label)
            };
            sb.push_str(prefix);
            sb.push_str(&label);
            sb.push('\n');
        }

        sb.push('\n');
        sb.push_str(&styles::HELP.render(t("oauth_help")));
        sb
    }

    /// The provider line and the sign-in URL, which both modes start with.
    fn render_url(&self, sb: &mut String) {
        let provider = Style::new().bold(true).fg(styles::COLOR_HIGHLIGHT);
        sb.push_str(&provider.render(&format!("  ✦ {} OAuth", self.provider_name)));
        sb.push_str("\n\n");

        sb.push_str(&info_label().render(self.locale.t("oauth_auth_url")));
        sb.push('\n');

        let url_style = Style::new().fg(Color::Ansi(252));
        for line in wrap_text(&self.auth_url, (self.width - 6).max(40)) {
            sb.push_str("  ");
            sb.push_str(&url_style.render(&line));
            sb.push('\n');
        }
        sb.push('\n');
    }

    /// `renderRemoteMode`.
    fn render_remote_mode(&self, sb: &mut String) {
        let t = |key: &'static str| self.locale.t(key);
        self.render_url(sb);

        sb.push_str(&styles::HELP.render(t("oauth_remote_hint")));
        sb.push_str("\n\n");

        sb.push_str(&info_label().render(t("oauth_callback_url")));
        sb.push('\n');

        if self.input_active {
            sb.push_str(&self.callback_input.view());
            sb.push('\n');
            sb.push_str(&styles::HELP.render(&format!(
                "  {} • {}",
                t("enter_submit"),
                t("esc_cancel")
            )));
        } else {
            sb.push_str(&styles::HELP.render(t("oauth_press_c")));
        }

        sb.push_str("\n\n");
        sb.push_str(&styles::WARNING.render(t("oauth_waiting")));
    }

    /// `renderDeviceMode`.
    fn render_device_mode(&self, sb: &mut String) {
        let t = |key: &'static str| self.locale.t(key);
        self.render_url(sb);

        if !self.user_code.trim().is_empty() {
            sb.push_str(&info_label().render(t("oauth_user_code")));
            sb.push('\n');
            let code = Style::new()
                .bold(true)
                .fg(styles::COLOR_WHITE)
                .bg(styles::COLOR_PRIMARY)
                .padding(0, 1);
            sb.push_str("  ");
            sb.push_str(&code.render(&self.user_code));
            sb.push_str("\n\n");
        }

        sb.push_str(&styles::HELP.render(t("oauth_device_hint")));
        sb.push('\n');
        if self.expires_in > 0 {
            let expires = fill(t("oauth_device_expires"), &[&self.expires_in.to_string()]);
            sb.push_str(&styles::HELP.render(&expires));
            sb.push('\n');
        }
        sb.push('\n');
        sb.push_str(&styles::WARNING.render(t("oauth_waiting")));
        sb.push('\n');
        sb.push_str(&styles::HELP.render(t("oauth_press_esc")));
    }
}

/// The style of the section labels.
const fn info_label() -> Style {
    Style::new().bold(true).fg(styles::COLOR_INFO)
}

/// How long a poll runs: as long as the sign-in lasts, up to a day, else
/// upstream's defaults.
fn poll_timeout(expires_in: i64, device_flow: bool) -> Duration {
    if let Ok(secs) = u64::try_from(expires_in)
        && secs > 0
    {
        return Duration::from_secs(secs).min(MAX_POLL_TIMEOUT);
    }
    if device_flow {
        DEVICE_POLL_TIMEOUT
    } else {
        DEFAULT_POLL_TIMEOUT
    }
}

/// `shouldAcceptOAuthStart`: whether a start belongs to the current
/// sign-in.
fn should_accept_start(start: &Start, generation: u64) -> bool {
    start.generation == generation
}

/// `shouldAcceptOAuthPoll`: whether a poll belongs to the sign-in being
/// waited for.
fn should_accept_poll(poll: &Poll, auth_state: &str, generation: u64, state: State) -> bool {
    if poll.generation != generation {
        return false;
    }
    if poll.state.is_empty() || poll.state != auth_state {
        return false;
    }
    state == State::Remote
}

/// `shouldFailOAuthStatusPoll`: whether this many failed polls in a row
/// end the sign-in.
fn should_fail_status_poll(consecutive_errors: i64, max_errors: i64) -> bool {
    if max_errors <= 0 {
        return consecutive_errors > 0;
    }
    consecutive_errors >= max_errors
}

/// `wrapText`: `s` in lines of at most `max_width` characters.
fn wrap_text(s: &str, max_width: i64) -> Vec<String> {
    let Ok(max_width) = usize::try_from(max_width) else {
        return vec![s.to_owned()];
    };
    if max_width == 0 {
        return vec![s.to_owned()];
    }
    let chars: Vec<char> = s.chars().collect();
    chars
        .chunks(max_width)
        .map(|c| c.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_tab(auth_state: &str, generation: u64) -> OAuthTab {
        let mut m = OAuthTab::new(None, Locale::default(), Platform::none());
        m.state = State::Remote;
        m.auth_state = auth_state.to_owned();
        m.poll_generation = generation;
        m.ready = true;
        m.viewport = Viewport::new(80, 24);
        m.refresh();
        m
    }

    // Ports TestShouldAcceptOAuthPollFiltersStaleMessages.
    #[test]
    fn should_accept_oauth_poll_filters_stale_messages() {
        let msg = Poll {
            state: "state-a".to_owned(),
            generation: 1,
            done: true,
            message: "ok".to_owned(),
            err: None,
        };
        assert!(!should_accept_poll(&msg, "state-a", 2, State::Remote));
        assert!(!should_accept_poll(&msg, "state-b", 1, State::Remote));
        assert!(!should_accept_poll(&msg, "state-a", 1, State::Idle));
        assert!(should_accept_poll(&msg, "state-a", 1, State::Remote));
    }

    // Ports TestShouldAcceptOAuthStartFiltersStaleMessages.
    #[test]
    fn should_accept_oauth_start_filters_stale_messages() {
        let msg = Start {
            state: "state-a".to_owned(),
            generation: 1,
            url: "https://example.com".to_owned(),
            ..Start::default()
        };
        assert!(!should_accept_start(&msg, 2));
        assert!(should_accept_start(&msg, 1));
    }

    // Ports TestShouldFailOAuthStatusPoll.
    #[test]
    fn should_fail_oauth_status_poll() {
        assert!(!should_fail_status_poll(4, 5));
        assert!(should_fail_status_poll(5, 5));
        assert!(should_fail_status_poll(1, 0));
    }

    // Ports TestOAuthTabUpdateIgnoresStalePollMsg.
    #[test]
    fn update_ignores_stale_poll_msg() {
        let mut m = remote_tab("state-current", 2);
        let cmd = m.update(Msg::OAuthPoll(Poll {
            state: "state-old".to_owned(),
            generation: 1,
            done: true,
            message: "should be ignored".to_owned(),
            err: None,
        }));
        assert!(cmd.is_none());
        assert_eq!(m.state, State::Remote);
        assert_eq!(m.message, "");
    }

    // Ports TestOAuthTabUpdateAcceptsCurrentPollMsg.
    #[test]
    fn update_accepts_current_poll_msg() {
        let mut m = remote_tab("state-current", 3);
        let _ = m.update(Msg::OAuthPoll(Poll {
            state: "state-current".to_owned(),
            generation: 3,
            done: true,
            message: "Authentication successful".to_owned(),
            err: None,
        }));
        assert_eq!(m.state, State::Success);
    }

    // Ports TestOAuthTabEscRemoteIncrementsGenerationAndClearsState.
    #[test]
    fn esc_remote_increments_generation_and_clears_state() {
        let mut m = remote_tab("state-to-cancel", 4);
        m.auth_url = "https://example.com".to_owned();
        m.device_flow = true;
        let cmd = m.update(Msg::Key(Key::named("esc")));
        assert_eq!(m.state, State::Idle);
        assert_eq!(m.poll_generation, 5);
        assert!(m.auth_state.is_empty() && m.auth_url.is_empty() && !m.device_flow);
        // With no client, there is no session to cancel.
        assert!(cmd.is_none());
    }

    // Ports TestOAuthTabEscWithActiveCallbackInputCancelsRemoteSession.
    #[test]
    fn esc_with_active_callback_input_cancels_remote_session() {
        let mut m = remote_tab("state-to-cancel", 7);
        m.auth_url = "https://example.com".to_owned();
        m.device_flow = false;
        m.input_active = true;
        m.callback_input.focus();
        m.callback_input
            .set_value("https://callback.example/?code=abc&state=state-to-cancel");
        let cmd = m.update(Msg::Key(Key::named("esc")));
        assert_eq!(m.state, State::Idle);
        assert_eq!(m.poll_generation, 8);
        assert!(!m.input_active);
        assert_eq!(m.callback_input.value(), "");
        assert!(m.auth_state.is_empty() && m.auth_url.is_empty());
        assert!(cmd.is_none());
    }

    // Ports TestOAuthTabStaleStartIsIgnored.
    #[test]
    fn stale_start_is_ignored() {
        let mut m = remote_tab("", 2);
        m.state = State::Idle;
        let cmd = m.update(Msg::OAuthStart(Start {
            url: "https://example.com".to_owned(),
            state: "stale-state".to_owned(),
            generation: 1,
            ..Start::default()
        }));
        assert_eq!(m.state, State::Idle);
        assert!(cmd.is_none());
        assert_eq!(m.auth_state, "");
    }

    // Not upstream's: a poll stops once its sign-in is cancelled, and a
    // sign-in's expiry is polled for at most a day.
    #[tokio::test(start_paused = true)]
    async fn stale_polls_stop() {
        let server = crate::testing::Server::start(&[(
            "GET /v0/management/get-auth-status",
            r#"{"status":"wait"}"#,
        )])
        .await;
        let client = Client::new(&server.url(), "k");
        let mut m = OAuthTab::new(Some(client), Locale::default(), Platform::none());
        m.auth_state = "st".to_owned();
        m.poll_generation = 1;
        m.live_generation.store(1, Ordering::Relaxed);
        let Some(Cmd::Run(task)) = m.poll_status(1) else {
            panic!("no poll");
        };
        m.next_generation();
        assert!(task.await.is_none());
        assert!(server.requests().is_empty());

        assert_eq!(poll_timeout(i64::MAX, false), MAX_POLL_TIMEOUT);
        assert_eq!(poll_timeout(600, true), Duration::from_secs(600));
        assert_eq!(poll_timeout(0, true), DEVICE_POLL_TIMEOUT);
        assert_eq!(poll_timeout(-1, false), DEFAULT_POLL_TIMEOUT);
    }

    // Not upstream's: a URL wraps by characters, as upstream's wraps ASCII.
    #[test]
    fn wraps_text_by_characters() {
        assert_eq!(wrap_text("abcdef", 4), ["abcd", "ef"]);
        assert_eq!(wrap_text("abcd", 4), ["abcd"]);
        assert!(wrap_text("", 4).is_empty());
        assert_eq!(wrap_text("äöüß", 3), ["äöü", "ß"]);
        assert_eq!(wrap_text("ab", 0), ["ab"]);
    }
}
