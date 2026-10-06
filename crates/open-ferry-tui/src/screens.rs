//! Screen tests: the app driven through the key paths of each tab against
//! a loopback management server, drawn on ratatui's test backend, and
//! compared with the screens upstream's TUI showed for the same steps.
//!
//! The screens in `testdata/screens` were recorded by driving upstream's
//! app (v8.0.15, with Go 1.26.4) the same way, against a server with the
//! same routes, keeping the last lines of each view that fit the terminal,
//! cut to its width, without escape sequences or trailing blanks.
//! `requests.txt` lists the requests that server took.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::ansi;
use crate::app::{App, Msg, Platform};
use crate::keys::Key;
use crate::loghook::LogHook;
use crate::tea::Cmd;
use crate::testing::Server;

/// How long the driver waits for a command, as the recording did. A
/// command still running then is dropped: the logs tab waiting for a hook
/// line, and an OAuth poll.
const TIMEOUT: Duration = Duration::from_millis(300);

/// The port of the server the screens were recorded against.
const RECORDED_PORT: &str = "58121";

/// The recording server's routes.
const ROUTES: [(&str, &str); 14] = [
    (
        "GET /v0/management/config",
        r#"{"debug":false,"proxy-url":"socks5://127.0.0.1:1080","request-retry":3,"max-retry-interval":30,"logging-to-file":true,"logs-max-total-size-mb":0,"error-logs-max-files":10,"usage-statistics-enabled":true,"request-log":false,"quota-exceeded":{"switch-project":true,"switch-preview-model":true},"routing":{"strategy":"fill-first"},"ws-auth":false,"port":8317,"host":"127.0.0.1"}"#,
    ),
    (
        "GET /v0/management/auth-files",
        r#"{"files":[{"name":"claude-user@example.com.json","channel":"claude","email":"user@example.com","disabled":false,"status":"active","auth_type":"oauth","priority":2,"created_at":"2026-01-02T03:04:05Z","file_name":"claude-user@example.com.json"},{"name":"codex-a-very-long-file-name-for-testing.json","channel":"codex","email":"someone.with.a.long.address@example.org","disabled":true,"status":"disabled","prefix":"team","priority":1.5}]}"#,
    ),
    (
        "GET /v0/management/api-keys",
        r#"{"api-keys":["sk-test-1234567890","short","abcdefghij"]}"#,
    ),
    (
        "GET /v0/management/gemini-api-key",
        r#"{"gemini-api-key":[{"api-key":"AIzaDummyKey000000","prefix":"g","base-url":"https://gemini.example.invalid"}]}"#,
    ),
    (
        "GET /v0/management/claude-api-key",
        r#"{"claude-api-key":[{"api-key":"sk-ant-dummy-0000"}]}"#,
    ),
    (
        "GET /v0/management/codex-api-key",
        r#"{"codex-api-key":null}"#,
    ),
    (
        "GET /v0/management/openai-compatibility",
        r#"{"openai-compatibility":[{"name":"local","base-url":"http://127.0.0.1:9/v1","prefix":"loc"}]}"#,
    ),
    (
        "GET /v0/management/logs",
        r#"{"lines":["[2026-01-01 00:00:00] [--------] [info ] [main.go:10] server started","[2026-01-01 00:00:01] [--------] [warn ] [auth.go:20] token expires soon","[2026-01-01 00:00:02] [abcd1234] [error] [proxy.go:30] upstream failed","[2026-01-01 00:00:03] [--------] [debug] [x.go:1] detail"],"latest-timestamp":1767225603}"#,
    ),
    (
        "GET /v0/management/anthropic-auth-url",
        r#"{"status":"ok","url":"https://auth.example.invalid/oauth/authorize?code=true&client_id=dummy-client&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback&scope=org%3Acreate_api_key&state=st-1","state":"st-1"}"#,
    ),
    (
        "GET /v0/management/codex-auth-url",
        r#"{"status":"ok","url":"https://auth.example.invalid/device","state":"st-2","user_code":"ABCD-1234","expires_in":600}"#,
    ),
    ("GET /v0/management/get-auth-status", r#"{"status":"wait"}"#),
    ("PUT /v0/management/debug", r#"{"status":"ok"}"#),
    (
        "PATCH /v0/management/auth-files/fields",
        r#"{"status":"ok"}"#,
    ),
    ("DELETE /v0/management/oauth-session", r#"{"status":"ok"}"#),
];

/// What the app asked the platform to do.
#[derive(Default)]
struct Recorded {
    opened: Mutex<Vec<String>>,
    copied: Mutex<Vec<String>>,
}

fn recording() -> (Platform, Arc<Recorded>) {
    let recorded = Arc::new(Recorded::default());
    let opened = Arc::clone(&recorded);
    let copied = Arc::clone(&recorded);
    let platform = Platform {
        open_url: Arc::new(move |url| {
            lock(&opened.opened).push(url.to_owned());
        }),
        copy: Arc::new(move |text| {
            lock(&copied.copied).push(text.to_owned());
            Ok(())
        }),
    };
    (platform, recorded)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Drives an app as the recording did: each command runs to its message
/// before the next input, a batch's one after another.
struct Driver {
    app: App,
    terminal: Terminal<TestBackend>,
    port: u16,
}

impl Driver {
    fn new(app: App, width: u16, height: u16, port: u16) -> Self {
        Self {
            app,
            terminal: Terminal::new(TestBackend::new(width, height)).unwrap(),
            port,
        }
    }

    async fn send(&mut self, msg: Msg) {
        let cmd = self.app.update(msg);
        self.run(cmd).await;
    }

    fn run(&mut self, cmd: Option<Cmd>) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(async move {
            match cmd {
                Some(Cmd::Run(task)) => {
                    if let Ok(Some(msg)) = tokio::time::timeout(TIMEOUT, task).await {
                        self.send(msg).await;
                    }
                }
                Some(Cmd::Batch(cmds)) => {
                    for cmd in cmds {
                        self.run(Some(cmd)).await;
                    }
                }
                Some(Cmd::Tick(..) | Cmd::Quit) | None => {}
            }
        })
    }

    async fn init(&mut self) {
        let cmd = self.app.init();
        self.run(cmd).await;
    }

    async fn resize(&mut self) {
        let size = self.terminal.size().unwrap();
        self.send(Msg::Resize {
            width: i64::from(size.width),
            height: i64::from(size.height),
        })
        .await;
    }

    async fn keys(&mut self, keys: &[&'static str]) {
        for key in keys {
            let key = match *key {
                "enter" | "esc" | "tab" | "shift+tab" | "up" | "down" | "ctrl+u" => Key::named(key),
                text => Key::text(text),
            };
            self.send(Msg::Key(key)).await;
        }
    }

    async fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.send(Msg::Key(Key::text(&c.to_string()))).await;
        }
    }

    /// Draws the app and checks the screen against the recorded one.
    fn screen(&mut self, name: &str, recorded: &str) {
        let view = self.app.view();
        // The test backend keeps the cells a wide character covers as they
        // were, where a terminal blanks them when the character goes, so
        // each screen is drawn whole.
        self.terminal.clear().unwrap();
        self.terminal
            .draw(|frame| ansi::render(&view, frame.area(), frame.buffer_mut()))
            .unwrap();
        let got = ansi::buffer_text(self.terminal.backend().buffer());
        let want = recorded.replace(RECORDED_PORT, &self.port.to_string());
        assert_eq!(
            got.trim_end_matches('\n'),
            want.trim_end_matches('\n'),
            "screen {name}"
        );
    }
}

macro_rules! screen {
    ($driver:expr, $name:literal) => {
        $driver.screen(
            $name,
            include_str!(concat!("../testdata/screens/", $name, ".txt")),
        )
    };
}

// Not upstream's: client mode, from the key gate through every tab's key
// paths, then the language switch, shows what upstream showed and makes
// the requests it made.
#[tokio::test]
async fn client_mode_shows_as_upstream_does() {
    let server = Server::start(&ROUTES).await;
    let (platform, recorded) = recording();
    let app = App::new(&server.url(), "", None, platform);
    let mut d = Driver::new(app, 100, 30, server.port());
    d.init().await;
    d.resize().await;
    screen!(d, "a01-gate");
    d.keys(&["enter"]).await;
    screen!(d, "a02-gate-empty");
    d.type_text("secret-key").await;
    screen!(d, "a03-gate-typed");
    d.keys(&["enter"]).await;
    screen!(d, "a04-dashboard");

    d.keys(&["tab"]).await;
    screen!(d, "a05-config");
    d.keys(&["down", "down", "enter"]).await;
    screen!(d, "a06-config-toggled");
    d.keys(&["down", "enter"]).await;
    screen!(d, "a07-config-editing");
    d.keys(&["esc", "down", "enter", "ctrl+u"]).await;
    d.type_text("abc").await;
    screen!(d, "a08-config-int-typed");
    d.keys(&["enter"]).await;
    screen!(d, "a09-config-invalid-int");
    d.keys(&["down"; 12]).await;
    screen!(d, "a10-config-bottom");

    d.keys(&["tab"]).await;
    screen!(d, "a11-auth");
    d.keys(&["enter"]).await;
    screen!(d, "a12-auth-expanded");
    d.keys(&["d"]).await;
    screen!(d, "a13-auth-confirm");
    d.keys(&["n", "1"]).await;
    screen!(d, "a14-auth-edit");
    d.keys(&["esc", "down"]).await;
    screen!(d, "a15-auth-second");
    d.keys(&["enter"]).await;
    screen!(d, "a16-auth-second-expanded");

    d.keys(&["tab"]).await;
    screen!(d, "a17-keys");
    d.keys(&["a"]).await;
    screen!(d, "a18-keys-adding");
    d.keys(&["esc", "d"]).await;
    screen!(d, "a19-keys-confirm");
    d.keys(&["n", "e"]).await;
    screen!(d, "a20-keys-editing");

    d.keys(&["esc", "tab"]).await;
    screen!(d, "a21-oauth");
    d.keys(&["down"]).await;
    screen!(d, "a22-oauth-codex");
    d.keys(&["up", "enter"]).await;
    screen!(d, "a23-oauth-remote");
    d.type_text("http://localhost/cb?code=1").await;
    screen!(d, "a24-oauth-typed");
    d.keys(&["esc"]).await;
    screen!(d, "a25-oauth-cancelled");
    d.keys(&["down", "enter"]).await;
    screen!(d, "a26-oauth-device");

    d.keys(&["esc", "tab"]).await;
    screen!(d, "a27-logs");
    d.keys(&["3"]).await;
    screen!(d, "a28-logs-warn");
    d.keys(&["q"]).await;
    screen!(d, "a29-logs-q");
    d.keys(&["L"]).await;
    screen!(d, "a30-logs-zh");
    d.keys(&["tab"]).await;
    screen!(d, "a31-dashboard-zh");
    d.keys(&["tab"]).await;
    screen!(d, "a32-config-zh");
    d.keys(&["L", "q"]).await;

    // Upstream's polls kept asking for the sign-in's status after the
    // recording moved on; these stop when their sign-in is cancelled.
    let want: Vec<String> = include_str!("../testdata/screens/requests.txt")
        .lines()
        .filter(|line| line.contains("auth=\"Bearer secret-key\""))
        .filter(|line| !line.contains("get-auth-status"))
        .map(|line| line.replace("auth=\"Bearer secret-key\"", "auth=Bearer secret-key"))
        .collect();
    assert_eq!(server.take_requests(), want);
    assert_eq!(
        *lock(&recorded.opened),
        [
            "https://auth.example.invalid/oauth/authorize?code=true&client_id=dummy-client&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback&scope=org%3Acreate_api_key&state=st-1",
            "https://auth.example.invalid/device",
        ]
    );
    assert!(lock(&recorded.copied).is_empty());
}

// Not upstream's: standalone mode starts signed in, shows the logs tab and
// the hook's lines, and a narrow terminal cuts the dashboard and config as
// upstream's did.
#[tokio::test]
async fn standalone_mode_shows_as_upstream_does() {
    let server = Server::start(&ROUTES).await;
    let hook = LogHook::new(10);

    let (platform, _) = recording();
    let app = App::new(&server.url(), "pw", Some(hook.clone()), platform);
    let mut d = Driver::new(app, 60, 15, server.port());
    d.resize().await;
    d.init().await;
    screen!(d, "b01-standalone-dashboard");
    d.keys(&["shift+tab"]).await;
    screen!(d, "b02-standalone-logs");
    for line in [
        "[2026-01-01 00:00:00] [--------] [info ] [main.go:1] hello",
        "[2026-01-01 00:00:01] [--------] [error] [main.go:2] broken",
    ] {
        d.send(Msg::LogLine(line.to_owned())).await;
    }
    screen!(d, "b03-standalone-logs-lines");

    let (platform, _) = recording();
    let app = App::new(&server.url(), "pw", Some(hook), platform);
    let mut d = Driver::new(app, 40, 12, server.port());
    d.resize().await;
    d.init().await;
    screen!(d, "c01-narrow-dashboard");
    d.keys(&["tab"]).await;
    screen!(d, "c02-narrow-config");

    let want: Vec<String> = include_str!("../testdata/screens/requests.txt")
        .lines()
        .filter(|line| line.contains("auth=\"Bearer pw\""))
        .map(|line| line.replace("auth=\"Bearer pw\"", "auth=Bearer pw"))
        .collect();
    assert_eq!(server.take_requests(), want);
}

// Not upstream's: in standalone mode the logs tab shows lines as the hook
// takes them, without polling the server.
#[tokio::test]
async fn standalone_logs_come_from_the_hook() {
    let server = Server::start(&ROUTES).await;
    let hook = LogHook::new(10);
    let (platform, _) = recording();
    let mut app = App::new(&server.url(), "pw", Some(hook.clone()), platform);
    let _ = app.update(Msg::Resize {
        width: 60,
        height: 15,
    });
    let _ = app.update(Msg::Key(Key::named("shift+tab")));
    let Some(Cmd::Run(wait)) = app.update(Msg::LogsTick).or_else(|| {
        // A tick does nothing with a hook; the tab waits on the hook.
        app.update(Msg::LogLine("first".to_owned()))
    }) else {
        panic!("the logs tab doesn't wait for the hook");
    };
    hook.send("[info ] second\n");
    let Some(msg) = wait.await else {
        panic!("no line");
    };
    let _ = app.update(msg);
    let view = app.view();
    assert!(
        view.contains("first") && view.contains("[info ] second"),
        "{view}"
    );
    assert!(server.requests().is_empty());
}

// Not upstream's: the keys tab copies the selected key, and adds, edits
// and deletes keys with upstream's requests.
#[tokio::test]
async fn copies_and_writes_keys() {
    let server = Server::start(&ROUTES).await;
    server.set("PATCH /v0/management/api-keys", r#"{"status":"ok"}"#);
    server.set("DELETE /v0/management/api-keys", r#"{"status":"ok"}"#);
    let (platform, recorded) = recording();
    let app = App::new(&server.url(), "secret-key", None, platform);
    let mut d = Driver::new(app, 100, 30, server.port());
    d.resize().await;
    d.keys(&["enter", "tab", "tab", "tab", "down", "c"]).await;
    assert_eq!(*lock(&recorded.copied), ["short"]);
    assert!(d.app.view().contains("✓ Copied to clipboard"));
    let _ = server.take_requests();

    d.keys(&["a"]).await;
    d.type_text("sk-new").await;
    d.keys(&["enter"]).await;
    d.keys(&["e", "ctrl+u"]).await;
    d.type_text("renamed").await;
    d.keys(&["enter", "d", "y"]).await;
    let writes: Vec<String> = server
        .take_requests()
        .into_iter()
        .filter(|r| !r.starts_with("GET "))
        .collect();
    assert_eq!(
        writes,
        [
            r#"PATCH /v0/management/api-keys auth=Bearer secret-key body={"new":"sk-new","old":null}"#,
            r#"PATCH /v0/management/api-keys auth=Bearer secret-key body={"index":1,"value":"renamed"}"#,
            "DELETE /v0/management/api-keys?index=1 auth=Bearer secret-key body=",
        ]
    );
    assert!(d.app.view().contains("✓ API Key deleted"));
}
