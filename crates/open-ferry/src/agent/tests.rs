//! Tests of the agent commands, each against a config in a temporary
//! directory: with no server running for it (its port on 127.0.0.1 held
//! by a socket that doesn't listen), and with a test server running for
//! it (the management API's and the dashboard's routes, on an ephemeral
//! port of 127.0.0.1), and of the MCP server over an in-memory pipe. Every
//! key here is a dummy.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::Query;
use axum::http::Method;
use axum::routing::get as get_route;
use chrono::Utc;
use open_ferry_core::auth::synthesizer::SynthesisContext;
use open_ferry_core::auth::synthesizer::file::synthesize_auth_file;
use open_ferry_core::auth::{Auth, FileStore};
use open_ferry_core::config::{AuthFile, Config};
use open_ferry_core::manager::{Manager, Settings};
use open_ferry_core::registry::ModelRegistry;
use open_ferry_dashboard::Ledger;
use open_ferry_management::{CredentialSync, FileConfigWriter, ManagementState, SyncFuture};
use rmcp::ServiceExt as _;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpSocket};

use super::api::{Body, Remote};
use super::clients::SetupInput;
use super::config::{GetInput, ReplaceInput, SetInput, Source, UnsetInput};
use super::credentials::{ListInput as CredentialsList, LoginInput, TargetInput};
use super::keys::{AddInput, ListInput as KeysList, RemoveInput};
use super::mcp::{CONFIG_URI, DOCS_URI, Server, tool_result};
use super::target::Env;
use super::{Caller, Command, Context, Failure, Outcome, exit, perform};

/// The management key the configs hold.
const KEY: &str = "test-management-key-0123456789";

/// The client key the configs hold.
const CLIENT_KEY: &str = "sk-test-client-key-abcdefghijklmnop";

/// The management key a test server takes from its environment.
const PASSWORD: &str = "test-env-password-0123456789";

/// What a management key the server hashed looks like: a config with it
/// has no plain key to call the server with.
const HASHED: &str = "$2a$10$abcdefghijklmnopqrstuuabcdefghijklmnopqrstuvwxyz01234";

/// The config of a server on 127.0.0.1:`port`, with the management key
/// `key`, the client key, and `auth_dir`.
fn config_text(port: u16, key: Option<&str>, auth_dir: &Path) -> String {
    let mut text = format!("config-version: 8\nserver:\n  host: \"127.0.0.1\"\n  port: {port}\n");
    if let Some(key) = key {
        text.push_str(&format!("management:\n  secret-key: \"{key}\"\n"));
    }
    text.push_str(&format!(
        "access:\n  api-keys:\n    - \"{CLIENT_KEY}\"\noauth:\n  auth-dir: '{}'\n",
        auth_dir.display().to_string().replace('\\', "/")
    ));
    text
}

/// A config in a temporary directory, and its auth directory.
struct Setup {
    dir: tempfile::TempDir,
    path: PathBuf,
    auth_dir: PathBuf,
}

impl Setup {
    /// A config for a server on 127.0.0.1:`port`, with `key`.
    fn new(port: u16, key: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let auth_dir = dir.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, config_text(port, key, &auth_dir)).unwrap();
        Self {
            dir,
            path,
            auth_dir,
        }
    }

    fn text(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap()
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    }
}

/// A config whose port on 127.0.0.1 nothing listens on, while it lives.
struct Offline {
    setup: Setup,
    _socket: TcpSocket,
}

fn offline(key: Option<&str>) -> Offline {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = socket.local_addr().unwrap().port();
    Offline {
        setup: Setup::new(port, key),
        _socket: socket,
    }
}

/// The context of `caller` for the config at `path`, with no key in the
/// environment, no terminal and no confirmation.
fn context(path: &Path, caller: Caller) -> Context {
    Context {
        path: path.to_owned(),
        env: Env::default(),
        key_file: None,
        yes: false,
        ask: None,
        say: None,
        caller,
    }
}

fn cli(path: &Path) -> Context {
    context(path, Caller::Cli)
}

fn confirmed(path: &Path, caller: Caller) -> Context {
    Context {
        yes: true,
        ..context(path, caller)
    }
}

/// A command-line context with a terminal that answers `answer`, and how
/// many times it was asked.
fn asking(path: &Path, answer: bool) -> (Context, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&asked);
    let ctx = Context {
        ask: Some(Box::new(move |_question: &str| {
            count.fetch_add(1, Ordering::SeqCst);
            answer
        })),
        ..cli(path)
    };
    (ctx, asked)
}

fn get(path: &str) -> Command {
    Command::ConfigGet(GetInput {
        path: path.to_owned(),
    })
}

fn set(path: &str, value: &str) -> Command {
    Command::ConfigSet(SetInput {
        path: path.to_owned(),
        value: Source::Argument(value.to_owned()),
        string: false,
    })
}

fn set_from(path: &str, value: Source) -> Command {
    Command::ConfigSet(SetInput {
        path: path.to_owned(),
        value,
        string: false,
    })
}

fn unset(path: &str) -> Command {
    Command::ConfigUnset(UnsetInput {
        path: path.to_owned(),
    })
}

fn credential(name: &str) -> TargetInput {
    TargetInput {
        credential: name.to_owned(),
    }
}

async fn ok(ctx: &Context, command: Command) -> Outcome {
    match perform(ctx, command).await {
        Ok(outcome) => outcome,
        Err(failure) => panic!("failed: {failure:?}"),
    }
}

async fn fails(ctx: &Context, command: Command) -> Failure {
    match perform(ctx, command).await {
        Ok(outcome) => panic!("didn't fail: {}", outcome.json),
        Err(failure) => failure,
    }
}

/// Whether `secret` shows anywhere in `outcome`.
fn shows(outcome: &Outcome, secret: &str) -> bool {
    outcome.text.contains(secret) || outcome.json.to_string().contains(secret)
}

/// Whether `secret` shows anywhere in `failure`.
fn failure_shows(failure: &Failure, secret: &str) -> bool {
    failure.text().contains(secret) || serde_json::to_string(failure).unwrap().contains(secret)
}

// Not upstream's: with no server, a setting is read, set and unset in the
// file, and each change says what it changed and how to undo it.
#[tokio::test]
async fn settings_change_the_file_with_no_server() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let ctx = cli(&setup.path);

    let got = ok(&ctx, get("routing.strategy")).await;
    assert_eq!(got.json["set"], json!(false));
    assert!(got.text.starts_with("routing.strategy is not set"));

    let changed = ok(&ctx, set("routing.strategy", "fill-first")).await;
    assert_eq!(changed.code, exit::OK);
    assert_eq!(changed.json["action"], json!("set"));
    assert_eq!(changed.json["via"], json!("file"));
    assert_eq!(changed.json["changed"], json!(true));
    assert_eq!(
        changed.json["changes"],
        json!([{"path": "routing.strategy", "new": "fill-first"}])
    );
    let note = changed.json["note"].as_str().unwrap();
    assert!(
        note.starts_with("No server answers for this config"),
        "{note}"
    );
    assert!(
        changed
            .text
            .starts_with("Setting routing.strategy: done, in the config file.\n")
    );
    assert!(
        changed
            .text
            .contains("  routing.strategy: (not set) -> \"fill-first\"\n")
    );
    assert!(
        changed
            .text
            .contains("Undo it with `open-ferry config undo`.")
    );
    assert!(setup.text().contains("fill-first"));
    assert_eq!(
        ok(&ctx, get("routing.strategy")).await.json,
        json!({"path": "routing.strategy", "set": true, "value": "fill-first"})
    );

    let again = ok(&ctx, set("routing.strategy", "fill-first")).await;
    assert_eq!(again.json["changed"], json!(false));
    assert!(again.text.starts_with("Nothing to change"));

    ok(&ctx, set("routing.retry.request-retry", "5")).await;
    assert_eq!(
        ok(&ctx, get("routing.retry.request-retry")).await.json["value"],
        json!(5)
    );
    let text = Command::ConfigSet(SetInput {
        path: "requests.proxy-url".to_owned(),
        value: Source::Argument("5".to_owned()),
        string: true,
    });
    ok(&ctx, text).await;
    assert_eq!(
        ok(&ctx, get("requests.proxy-url")).await.json["value"],
        json!("5")
    );

    let removed = ok(&ctx, unset("routing.strategy")).await;
    assert_eq!(
        removed.json["changes"],
        json!([{"path": "routing.strategy", "old": "fill-first"}])
    );
    let again = ok(&ctx, unset("routing.strategy")).await;
    assert_eq!(again.json["changed"], json!(false));
    assert_eq!(
        again.text,
        "routing.strategy is not set; nothing to change.\n"
    );

    // A value the config can't hold is refused, and the file kept.
    let before = setup.text();
    let failure = fails(&ctx, set("server.port", "[1, 2]")).await;
    assert_eq!(failure.code, exit::FAILED);
    assert_eq!(setup.text(), before);
}

// Not upstream's: an unknown setting is refused with the nearest known
// one, for every command that takes a path, and nothing is changed.
#[tokio::test]
async fn unknown_paths_are_refused() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let before = setup.text();
    for ctx in [cli(&setup.path), confirmed(&setup.path, Caller::Mcp)] {
        for command in [
            get("routing.stratgy"),
            set("routing.stratgy", "fill-first"),
            unset("routing.stratgy"),
        ] {
            let failure = fails(&ctx, command).await;
            assert_eq!(failure.error, "unknown_path");
            assert_eq!(failure.code, exit::USAGE);
            assert!(failure.text().contains("routing.strategy"), "{failure:?}");
        }
    }
    assert_eq!(setup.text(), before);
    assert!(!super::change::read_config(&setup.path).unwrap().is_empty());
    assert!(!open_ferry_core::config::save::backup_path(&setup.path).exists());
}

// Not upstream's: undo puts the last change back, and an undo of the undo
// redoes it; diff shows what the last change made.
#[tokio::test]
async fn undo_and_undo_of_undo() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let ctx = cli(&setup.path);
    assert_eq!(fails(&ctx, Command::ConfigUndo).await.error, "no_backup");
    assert_eq!(fails(&ctx, Command::ConfigDiff).await.error, "no_backup");
    let original = setup.text();

    ok(&ctx, set("routing.strategy", "fill-first")).await;
    let changed = setup.text();
    let diffed = ok(&ctx, Command::ConfigDiff).await;
    assert_eq!(
        diffed.json["changes"],
        json!([{"path": "routing.strategy", "new": "fill-first"}])
    );
    assert!(
        diffed
            .text
            .contains("routing.strategy: (not set) -> \"fill-first\"")
    );

    let undone = ok(&ctx, Command::ConfigUndo).await;
    assert_eq!(undone.json["action"], json!("undo"));
    assert_eq!(undone.json["via"], json!("file"));
    assert_eq!(
        undone.json["changes"],
        json!([{"path": "routing.strategy", "old": "fill-first"}])
    );
    assert!(
        undone
            .text
            .contains("Run `open-ferry config undo` again to redo it.")
    );
    assert_eq!(setup.text(), original);

    let redone = ok(&ctx, Command::ConfigUndo).await;
    assert_eq!(
        redone.json["changes"],
        json!([{"path": "routing.strategy", "new": "fill-first"}])
    );
    assert_eq!(setup.text(), changed);

    // As a tool, the hint names the tool.
    let tool = ok(&context(&setup.path, Caller::Mcp), Command::ConfigUndo).await;
    assert_eq!(
        tool.json["undo"],
        json!("Call config_undo again to redo it.")
    );
}

/// A command-line context whose terminal, when asked, appends a comment to
/// `file` and answers yes.
fn asking_and_editing(path: &Path, file: &Path) -> Context {
    let file = file.to_owned();
    Context {
        ask: Some(Box::new(move |_question: &str| {
            let mut text = std::fs::read_to_string(&file).unwrap();
            text.push_str("# changed while asked\n");
            std::fs::write(&file, text).unwrap();
            true
        })),
        ..cli(path)
    }
}

// Not upstream's: an undo of a config changed since the last change that
// kept a backup, as by a hand edit, loses that edit too, so it is refused
// with changed_since unless confirmed; one whose backup changed after it
// was asked about is refused with config_changed, and nothing changes.
#[tokio::test]
async fn undo_of_a_changed_config_needs_a_confirmation() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let path = setup.path.clone();
    let backup = setup.dir.path().join("config.yaml.bak");
    ok(&cli(&path), set("routing.strategy", "fill-first")).await;
    let edited = setup.text().replace("fill-first", "round-robin");
    std::fs::write(&path, &edited).unwrap();

    let failure = fails(&cli(&path), Command::ConfigUndo).await;
    assert_eq!(failure.error, "changed_since");
    assert_eq!(failure.code, exit::FAILED);
    assert!(failure.message.contains("as by a hand edit"));
    assert!(failure.hint.as_deref().unwrap().contains("--yes"));
    let would = failure.would.clone().unwrap();
    assert_eq!(
        would["changes"],
        json!([{"path": "routing.strategy", "old": "round-robin"}])
    );
    assert!(
        would["reasons"][0]
            .as_str()
            .unwrap()
            .contains("undoing loses that change too")
    );
    assert_eq!(setup.text(), edited);
    let tool = fails(&context(&path, Caller::Mcp), Command::ConfigUndo).await;
    assert_eq!(tool.error, "changed_since");
    assert!(tool.hint.as_deref().unwrap().contains("confirm: true"));

    // A terminal is asked, and a no changes nothing.
    let (ctx, asked) = asking(&path, false);
    assert_eq!(fails(&ctx, Command::ConfigUndo).await.error, "declined");
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_eq!(setup.text(), edited);

    // A backup or file that changes while it is asked about is refused.
    let saved = std::fs::read_to_string(&backup).unwrap();
    let failure = fails(&asking_and_editing(&path, &backup), Command::ConfigUndo).await;
    assert_eq!(failure.error, "config_changed");
    assert_eq!(setup.text(), edited);
    std::fs::write(&backup, &saved).unwrap();
    let failure = fails(&asking_and_editing(&path, &path), Command::ConfigUndo).await;
    assert_eq!(failure.error, "config_changed");
    std::fs::write(&path, &edited).unwrap();

    // Confirmed, it goes ahead, and the hand edit goes with the change.
    let undone = ok(&confirmed(&path, Caller::Cli), Command::ConfigUndo).await;
    assert_eq!(undone.json["via"], json!("file"));
    assert!(!setup.text().contains("round-robin"));
    assert!(!setup.text().contains("fill-first"));
    // That undo was a recorded write, so undoing it needs no confirmation,
    // and puts the edit back.
    ok(&cli(&path), Command::ConfigUndo).await;
    assert_eq!(setup.text(), edited);
}

// Not upstream's: each sensitive setting needs --yes; with no terminal it
// changes nothing and says what it would change, masked; a terminal is
// asked, and a no changes nothing.
#[tokio::test]
async fn sensitive_settings_need_a_confirmation() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let secret_file = setup.file("secret", "a-new-management-key-value\n");
    let cases: Vec<(&str, Command)> = vec![
        (
            "management.allow-remote",
            set("management.allow-remote", "true"),
        ),
        ("server.host", set("server.host", "0.0.0.0")),
        ("server.tls", set("server.tls.enable", "true")),
        (
            "management.secret-key",
            set_from("management.secret-key", Source::File(secret_file.clone())),
        ),
        (
            "management.separate-address",
            set("management.separate-address", "127.0.0.1:9999"),
        ),
        ("server.host", unset("server.host")),
        (
            "server.trusted-proxies",
            set("server.trusted-proxies", r#"["10.0.0.0/8"]"#),
        ),
        ("last client key", unset("access.api-keys")),
        ("last client key", set("access.api-keys", r#"[""]"#)),
        ("last client key", set("access.api-keys", r#"["  ", ""]"#)),
        ("last client key", set("access", "{}")),
        ("last client key", unset("access")),
    ];
    for (reason, command) in cases {
        let before = setup.text();
        for caller in [Caller::Cli, Caller::Mcp] {
            let failure = fails(&context(&setup.path, caller), command.clone()).await;
            assert_eq!(failure.error, "needs_confirmation", "{reason}: {failure:?}");
            assert_eq!(failure.code, exit::CONFIRM);
            assert!(
                failure.message.contains(reason),
                "{reason}: {}",
                failure.message
            );
            assert!(failure.message.ends_with("Nothing was changed."));
            let flag = if caller == Caller::Cli {
                "--yes"
            } else {
                "confirm: true"
            };
            assert!(failure.message.contains(flag));
            let would = failure.would.clone().unwrap();
            assert!(!would["changes"].as_array().unwrap().is_empty(), "{reason}");
            assert!(!would["reasons"].as_array().unwrap().is_empty());
            assert!(failure.text().contains("It would change:"));
            assert!(!failure_shows(&failure, KEY));
            assert!(!failure_shows(&failure, CLIENT_KEY));
            assert!(!failure_shows(&failure, "a-new-management-key-value"));
            assert_eq!(setup.text(), before, "{reason}");
        }
        let (ctx, asked) = asking(&setup.path, false);
        let failure = fails(&ctx, command.clone()).await;
        assert_eq!(failure.error, "declined");
        assert_eq!(failure.code, exit::CONFIRM);
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        assert_eq!(setup.text(), before, "{reason}");
    }

    // A yes at the terminal, or --yes, goes ahead.
    let (ctx, asked) = asking(&setup.path, true);
    let changed = ok(&ctx, set("management.allow-remote", "true")).await;
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_eq!(changed.json["changed"], json!(true));
    let changed = ok(
        &confirmed(&setup.path, Caller::Cli),
        set("server.host", "0.0.0.0"),
    )
    .await;
    assert_eq!(
        changed.json["changes"],
        json!([{"path": "server.host", "old": "127.0.0.1", "new": "0.0.0.0"}])
    );
    let changed = ok(
        &confirmed(&setup.path, Caller::Mcp),
        set_from("management.secret-key", Source::File(secret_file)),
    )
    .await;
    assert!(!shows(&changed, KEY));
    assert!(!shows(&changed, "a-new-management-key-value"));
    assert!(setup.text().contains("a-new-management-key-value"));

    // A loopback host, or a setting that isn't sensitive, needs nothing.
    let ctx = cli(&setup.path);
    ok(&ctx, set("server.host", "localhost")).await;
    ok(&ctx, set("server.host", "::1")).await;
    ok(&ctx, set("routing.strategy", "fill-first")).await;
}

// Not upstream's: client keys are counted as the server counts them, so a
// list of blank keys is no key: making the list blank needs a
// confirmation, by `config set` or `config replace`, from the command line
// or as a tool over MCP, while a key given twice is still a key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blank_client_keys_count_as_none() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let port = u16::try_from(offline_port(setup)).unwrap();
    let before = setup.text();
    let blank = setup.file(
        "blank.yaml",
        &config_text(port, Some(KEY), &setup.auth_dir).replace(CLIENT_KEY, "  "),
    );
    for caller in [Caller::Cli, Caller::Mcp] {
        let failure = fails(
            &context(&setup.path, caller),
            Command::ConfigReplace(ReplaceInput {
                source: Source::File(blank.clone()),
            }),
        )
        .await;
        assert_eq!(failure.error, "needs_confirmation");
        let reasons = failure.would.clone().unwrap()["reasons"].to_string();
        assert!(reasons.contains("last client key"), "{reasons}");
        assert_eq!(setup.text(), before);
    }

    let server = Server::new(Ok(setup.path.clone()), Env::default(), None);
    let mut session = server_session(server).await;
    for value in [json!([""]), json!(["   "]), json!([])] {
        let result = session
            .call(
                "config_set",
                json!({"path": "access.api-keys", "value": value}),
            )
            .await;
        assert_eq!(result["isError"], json!(true), "{value}");
        assert_eq!(
            result["structuredContent"]["error"],
            json!("needs_confirmation")
        );
        assert!(
            result["structuredContent"]["would"]["reasons"]
                .to_string()
                .contains("last client key")
        );
    }
    let result = session
        .call("config_set", json!({"path": "access", "value": {}}))
        .await;
    assert_eq!(
        result["structuredContent"]["error"],
        json!("needs_confirmation")
    );
    let result = session
        .call("config_unset", json!({"path": "access"}))
        .await;
    assert_eq!(
        result["structuredContent"]["error"],
        json!("needs_confirmation")
    );
    assert_eq!(setup.text(), before);

    // A key given twice is still a key, so the list keeps one.
    let twice = setup.file("twice.json", &json!([CLIENT_KEY, CLIENT_KEY]).to_string());
    let changed = ok(
        &cli(&setup.path),
        set_from("access.api-keys", Source::File(twice)),
    )
    .await;
    assert_eq!(changed.json["changed"], json!(true));
    let status = ok(&cli(&setup.path), Command::Status).await;
    assert_eq!(status.json["client_keys"], json!(1));
}

// Not upstream's: a secret is never taken in the call itself, as an
// argument or a tool's JSON: only from standard input or a file.
#[tokio::test]
async fn secrets_are_never_taken_inline() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let before = setup.text();
    let provider = json!([{"name": "example", "base-url": "https://api.example.com", "keys": [{"api-key": "sk-provider-secret-value-1234"}]}]);
    for caller in [Caller::Cli, Caller::Mcp] {
        let ctx = confirmed(&setup.path, caller);
        let inline = |path: &str, value: Value| {
            Command::ConfigSet(SetInput {
                path: path.to_owned(),
                value: match caller {
                    Caller::Cli => Source::Argument(match &value {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    }),
                    Caller::Mcp => Source::Json(value),
                },
                string: false,
            })
        };
        for command in [
            inline("management.secret-key", json!("inline-secret-value")),
            inline("api-keys.codex", provider.clone()),
            inline("access.api-keys", json!(["sk-another-client-key-123456"])),
            Command::KeysAdd(AddInput {
                source: Some(Source::Argument("sk-another-client-key-123456".to_owned())),
                ..AddInput::default()
            }),
            Command::KeysRemove(RemoveInput {
                source: Some(Source::Argument(CLIENT_KEY.to_owned())),
                ..RemoveInput::default()
            }),
        ] {
            let failure = fails(&ctx, command).await;
            assert_eq!(failure.error, "secret_in_argument", "{failure:?}");
            assert_eq!(failure.code, exit::USAGE);
            assert!(!failure_shows(&failure, "inline-secret-value"));
            assert!(!failure_shows(&failure, "sk-provider-secret-value-1234"));
        }
        let failure = fails(
            &ctx,
            Command::ConfigReplace(ReplaceInput {
                source: Source::Argument(before.clone()),
            }),
        )
        .await;
        assert_eq!(failure.code, exit::USAGE);
        assert_eq!(setup.text(), before);
    }

    // From a file, or standard input, it is taken, and never shown.
    let file = setup.file("provider.json", &provider.to_string());
    let ctx = cli(&setup.path);
    let changed = ok(&ctx, set_from("api-keys.codex", Source::File(file))).await;
    assert!(
        !shows(&changed, "sk-provider-secret-value-1234"),
        "{}",
        changed.text
    );
    assert!(setup.text().contains("sk-provider-secret-value-1234"));
    let changed = ok(
        &confirmed(&setup.path, Caller::Cli),
        set_from(
            "management.secret-key",
            Source::Stdin("stdin-management-key-value\n".to_owned()),
        ),
    )
    .await;
    assert!(!shows(&changed, "stdin-management-key-value"));
    assert!(!shows(&changed, KEY));
    assert!(
        setup
            .text()
            .contains("secret-key: \"stdin-management-key-value\"")
            || setup
                .text()
                .contains("secret-key: stdin-management-key-value")
    );
}

// Not upstream's: secrets are masked in a command's text and JSON, and in
// the tools' results and the config resource; MANAGEMENT_PASSWORD and the
// key file are scrubbed wherever they would show.
#[tokio::test]
async fn output_is_masked() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let key_file = setup.file("key-file", "key-file-secret-value-123\n");
    std::fs::write(
        &setup.path,
        format!(
            "{}requests:\n  proxy-url: \"http://user:{PASSWORD}@proxy.example.com:8080\"\nrouting:\n  strategy: \"key-file-secret-value-123\"\n",
            setup.text()
        ),
    )
    .unwrap();
    let ctx = Context {
        env: Env {
            password: Some(PASSWORD.to_owned()),
            key_file: Some(key_file),
        },
        ..cli(&setup.path)
    };
    let secrets = [KEY, CLIENT_KEY, PASSWORD, "key-file-secret-value-123"];
    let shown = ok(&ctx, Command::ConfigShow).await;
    let listed = ok(&ctx, Command::KeysList(KeysList::default())).await;
    let proxy = ok(&ctx, get("requests.proxy-url")).await;
    let strategy = ok(&ctx, get("routing.strategy")).await;
    let secret = ok(&ctx, get("management.secret-key")).await;
    let status = ok(&ctx, Command::Status).await;
    let setup_curl = ok(
        &ctx,
        Command::ClientsSetup(SetupInput {
            client: "curl".to_owned(),
            model: None,
            shell: None,
            key_index: None,
            reveal: false,
        }),
    )
    .await;
    for outcome in [
        &shown,
        &listed,
        &proxy,
        &strategy,
        &secret,
        &status,
        &setup_curl,
    ] {
        for secret in secrets {
            assert!(!shows(outcome, secret), "{secret} in {}", outcome.text);
        }
    }
    let masked = open_ferry_dashboard::mask_client_key(CLIENT_KEY);
    assert!(shown.text.contains(&masked));
    assert_eq!(listed.json["keys"][0]["key"], json!(masked));
    assert_eq!(
        secret.json["value"],
        json!(open_ferry_dashboard::mask_client_key(KEY))
    );
    assert!(setup_curl.text.contains(&masked));

    // The same through the tools, and the config resource.
    let server = Server::new(Ok(setup.path.clone()), ctx.env.clone(), None);
    for (name, arguments) in [
        ("config_show", json!({})),
        ("keys_list", json!({})),
        ("config_get", json!({"path": "management.secret-key"})),
        ("config_get", json!({"path": "requests.proxy-url"})),
        ("status", json!({})),
        ("clients_setup", json!({"client": "openai-python"})),
    ] {
        let result = tool_result(server.call(name, arguments.as_object().cloned()).await);
        let text = serde_json::to_string(&result).unwrap();
        for secret in secrets {
            assert!(!text.contains(secret), "{secret} in {name}: {text}");
        }
    }
    let resource = server_session(server.clone()).await;
    let mut session = resource;
    let read = session
        .request("resources/read", json!({"uri": CONFIG_URI}))
        .await;
    let text = read["result"]["contents"][0]["text"].as_str().unwrap();
    assert!(text.contains(&masked), "{text}");
    for secret in secrets {
        assert!(!text.contains(secret), "{secret} in the resource");
    }
}

// Not upstream's: client keys are listed masked, added (made here, or
// from a file) and removed by index or from a file; a tool returns a
// new key only with confirm: true, else writes it to a new file.
#[tokio::test]
async fn keys_are_listed_added_and_removed() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let ctx = cli(&setup.path);
    let tool = context(&setup.path, Caller::Mcp);

    let listed = ok(&ctx, Command::KeysList(KeysList::default())).await;
    assert_eq!(listed.json["count"], json!(1));
    assert_eq!(listed.json["revealed"], json!(false));
    assert!(!shows(&listed, CLIENT_KEY));
    let reveal = Command::KeysList(KeysList { reveal: true });
    assert_eq!(
        fails(&ctx, reveal.clone()).await.error,
        "needs_confirmation"
    );
    let failure = fails(&confirmed(&setup.path, Caller::Mcp), reveal.clone()).await;
    assert_eq!(failure.code, exit::USAGE);
    let revealed = ok(&confirmed(&setup.path, Caller::Cli), reveal).await;
    assert_eq!(revealed.json["keys"][0]["key"], json!(CLIENT_KEY));
    assert!(revealed.text.contains(CLIENT_KEY));
    assert!(!shows(&revealed, KEY));

    // Made here, on the command line: shown once.
    let generate = || {
        Command::KeysAdd(AddInput {
            generate: true,
            ..AddInput::default()
        })
    };
    let added = ok(&ctx, generate()).await;
    let new_key = added.json["key"].as_str().unwrap().to_owned();
    assert!(
        new_key.starts_with("sk-") && new_key.len() == 46,
        "{new_key}"
    );
    assert_eq!(added.json["index"], json!(1));
    assert!(added.text.contains(&format!("\n  {new_key}\n")));
    assert!(
        added
            .text
            .contains("Undo it with `open-ferry config undo`.")
    );
    assert!(!shows(&added, CLIENT_KEY));
    assert!(setup.text().contains(&new_key));

    // As a tool: not without confirm, and then nothing changes.
    let before = setup.text();
    let failure = fails(&tool, generate()).await;
    assert_eq!(failure.error, "needs_confirmation");
    assert_eq!(setup.text(), before);
    let added = ok(&confirmed(&setup.path, Caller::Mcp), generate()).await;
    let tool_key = added.json["key"].as_str().unwrap().to_owned();
    assert!(setup.text().contains(&tool_key));

    // Or to a new file, which then holds the key the output doesn't show.
    let key_path = setup.dir.path().join("new-key.txt");
    let to_file = || {
        Command::KeysAdd(AddInput {
            generate: true,
            to_file: Some(key_path.clone()),
            ..AddInput::default()
        })
    };
    let added = ok(&tool, to_file()).await;
    let written = std::fs::read_to_string(&key_path).unwrap();
    let written = written.trim();
    assert!(written.starts_with("sk-"));
    assert!(!shows(&added, written));
    assert!(added.json.get("key").is_none());
    assert_eq!(
        added.json["key_file"],
        json!(key_path.display().to_string())
    );
    assert!(setup.text().contains(written));
    // A file that exists isn't replaced, and the config isn't changed.
    let before = setup.text();
    assert_eq!(fails(&tool, to_file()).await.error, "failed");
    assert_eq!(setup.text(), before);

    // From a file: a key in the list already is refused.
    let file = setup.file("client-key", &format!("{CLIENT_KEY}\n"));
    let from_file = |file: &Path| {
        Command::KeysAdd(AddInput {
            source: Some(Source::File(file.to_owned())),
            ..AddInput::default()
        })
    };
    assert_eq!(fails(&ctx, from_file(&file)).await.error, "exists");
    let other = setup.file("other-key", "sk-other-client-key-0123456789\n");
    let added = ok(&ctx, from_file(&other)).await;
    assert!(!shows(&added, "sk-other-client-key-0123456789"));
    assert!(added.json.get("key").is_none());

    // Removing needs a confirmation, by index or from a file.
    let before = setup.text();
    let by_index = |index: usize| {
        Command::KeysRemove(RemoveInput {
            index: Some(index),
            source: None,
        })
    };
    let failure = fails(&ctx, by_index(0)).await;
    assert_eq!(failure.error, "needs_confirmation");
    assert!(failure.message.contains("deletes client key 0"));
    assert!(!failure_shows(&failure, CLIENT_KEY));
    assert_eq!(setup.text(), before);
    assert_eq!(fails(&ctx, by_index(9)).await.error, "not_found");
    let yes = confirmed(&setup.path, Caller::Cli);
    let removed = ok(
        &yes,
        Command::KeysRemove(RemoveInput {
            index: None,
            source: Some(Source::File(other)),
        }),
    )
    .await;
    assert_eq!(removed.json["action"], json!("remove_key"));
    assert!(!setup.text().contains("sk-other-client-key-0123456789"));
    while ok(&yes, Command::KeysList(KeysList::default())).await.json["count"] != json!(1) {
        ok(&yes, by_index(1)).await;
    }
    // The last one says so.
    let failure = fails(&ctx, by_index(0)).await;
    assert!(
        failure.message.contains("last client key"),
        "{}",
        failure.message
    );
    ok(&yes, by_index(0)).await;
    let listed = ok(&ctx, Command::KeysList(KeysList::default())).await;
    assert_eq!(listed.json["count"], json!(0));
    assert!(listed.text.starts_with("No client keys"));
}

// Not upstream's: with no server, the commands that need one say so and
// how to start it, with exit code 4; status says it isn't running, and a
// client's setup is still printed from the config.
#[tokio::test]
async fn runtime_commands_need_the_server() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    for caller in [Caller::Cli, Caller::Mcp] {
        let ctx = confirmed(&setup.path, caller);
        for command in [
            Command::CredentialsList(CredentialsList::default()),
            Command::CredentialsEnable(credential("x")),
            Command::CredentialsDisable(credential("x")),
            Command::CredentialsResetQuota(credential("x")),
            Command::CredentialsRemove(credential("x")),
            Command::CredentialsLogin(LoginInput {
                provider: "codex".to_owned(),
                state: None,
                wait: false,
            }),
        ] {
            let failure = fails(&ctx, command).await;
            assert_eq!(failure.error, "not_running");
            assert_eq!(failure.code, exit::NOT_RUNNING);
            assert!(
                failure
                    .message
                    .contains("nothing answers at http://127.0.0.1:")
            );
            assert!(failure.hint.unwrap().contains("open-ferry --config"));
        }
    }
    let ctx = cli(&setup.path);
    let status = ok(&ctx, Command::Status).await;
    assert_eq!(status.code, exit::NOT_RUNNING);
    assert_eq!(status.json["running"], json!(false));
    assert_eq!(status.json["client_keys"], json!(1));
    assert!(status.text.contains("Server: not running"));

    let setup_out = ok(
        &ctx,
        Command::ClientsSetup(SetupInput {
            client: "curl".to_owned(),
            model: None,
            shell: None,
            key_index: None,
            reveal: false,
        }),
    )
    .await;
    assert_eq!(setup_out.code, exit::OK);
    assert!(setup_out.text.contains("127.0.0.1"));
    assert!(setup_out.text.contains("<model>"));

    // A Claude sign-in needs a confirmation before any server is asked;
    // claude-cli is never signed in to here.
    let claude = Command::CredentialsLogin(LoginInput {
        provider: "claude".to_owned(),
        state: None,
        wait: false,
    });
    let failure = fails(&context(&setup.path, Caller::Mcp), claude).await;
    assert_eq!(failure.error, "needs_confirmation");
    assert!(failure.message.contains("Anthropic's terms"));
    let cli_login = Command::CredentialsLogin(LoginInput {
        provider: "claude-cli".to_owned(),
        state: None,
        wait: false,
    });
    assert_eq!(fails(&ctx, cli_login).await.error, "refused");

    // A config that doesn't exist is said to be missing.
    let missing = cli(&setup.dir.path().join("nope.yaml"));
    assert_eq!(fails(&missing, get("server.port")).await.error, "not_found");
}

/// A [`CredentialSync`] that applies each change to the manager at once,
/// as the service does, without the model registry.
struct TestSync {
    manager: Manager,
}

impl TestSync {
    fn put(&self, auth: Auth) {
        if self.manager.get(&auth.id).is_some() {
            self.manager.update_unsaved(auth).unwrap();
        } else {
            self.manager.register_unsaved(auth).unwrap();
        }
    }

    fn remove_file(&self, path: &Path) {
        for auth in self.manager.list() {
            if auth.attribute("path").map(Path::new) == Some(path) {
                self.manager.remove(&auth.id);
            }
        }
    }
}

impl CredentialSync for TestSync {
    fn upsert(&self, auth: Auth) -> SyncFuture<'_> {
        self.put(auth);
        Box::pin(async { Ok(()) })
    }

    fn file_written(&self, file: AuthFile) -> SyncFuture<'_> {
        let dir = file.path.parent().unwrap_or(Path::new(""));
        let ctx = SynthesisContext::new(dir, Utc::now());
        match synthesize_auth_file(&ctx, &file.path, &file.data) {
            Ok(Some(auth)) => self.put(auth),
            _ => self.remove_file(&file.path),
        }
        Box::pin(async { Ok(()) })
    }

    fn file_removed(&self, path: PathBuf) -> SyncFuture<'_> {
        self.remove_file(&path);
        Box::pin(async { Ok(()) })
    }
}

/// A test server running for a config: the management API's and the
/// dashboard's routes on 127.0.0.1.
struct Live {
    setup: Setup,
    port: u16,
    state: ManagementState,
    manager: Manager,
}

/// Starts a test server for a config with the management key `key`, the
/// server taking `password` from its environment.
async fn live(key: Option<&str>, password: Option<&str>) -> Live {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let setup = Setup::new(port, key);
    let config = Arc::new(Config::load(&setup.path).unwrap());
    let registry = Arc::new(ModelRegistry::new());
    let store = Arc::new(FileStore::new(&setup.auth_dir));
    let manager = Manager::new(
        Settings::from(&*config),
        Arc::clone(&registry) as _,
        Some(Arc::clone(&store) as _),
    );
    let sync = Arc::new(TestSync {
        manager: manager.clone(),
    });
    let state = ManagementState::new(
        config,
        manager.clone(),
        registry,
        password.map(OsString::from),
    )
    .with_store(store)
    .with_sync(sync)
    .with_config_writer(Arc::new(FileConfigWriter::new(setup.path.clone())))
    .with_config_path(setup.path.clone());
    let app = open_ferry_management::router(state.clone()).merge(open_ferry_dashboard::router(
        state.clone(),
        Ledger::unavailable("tests have no ledger"),
    ));
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    Live {
        setup,
        port,
        state,
        manager,
    }
}

impl Live {
    /// The management API, called with `key`.
    fn remote(&self, key: &str) -> Remote {
        Remote::new(&format!("http://127.0.0.1:{}", self.port), key)
    }

    /// Writes the credential file `name` with `data` and registers it, as
    /// the watcher would.
    fn add_credential(&self, name: &str, data: &str) -> PathBuf {
        let path = self.setup.auth_dir.join(name);
        std::fs::write(&path, data).unwrap();
        let ctx = SynthesisContext::new(&self.setup.auth_dir, Utc::now());
        let auth = synthesize_auth_file(&ctx, &path, data.as_bytes())
            .unwrap()
            .unwrap();
        self.manager.register_unsaved(auth).unwrap();
        path
    }
}

// Not upstream's: while a server runs for the config and takes the key, a
// change goes through its management API, which the server then reads
// from; status says it runs, and undo goes through it too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_go_through_the_running_server() {
    let live = live(Some(KEY), None).await;
    let ctx = cli(&live.setup.path);

    let status = ok(&ctx, Command::Status).await;
    assert_eq!(status.code, exit::OK);
    assert_eq!(status.json["running"], json!(true));
    assert_eq!(status.json["management"], json!("ok"));
    assert_eq!(status.json["key_source"], json!("config"));
    assert_eq!(status.json["client_keys"], json!(1));
    assert!(status.text.contains("Server: running"));
    assert!(!shows(&status, KEY));

    let changed = ok(&ctx, set("routing.strategy", "fill-first")).await;
    assert_eq!(changed.json["via"], json!("server"));
    assert!(changed.json.get("note").is_none());
    assert!(
        changed
            .text
            .starts_with("Setting routing.strategy: done, through the running server.")
    );
    assert_eq!(live.state.config().routing.strategy, "fill-first");
    assert!(live.setup.text().contains("fill-first"));

    let removed = ok(&ctx, unset("routing.strategy")).await;
    assert_eq!(removed.json["via"], json!("server"));
    assert!(!live.setup.text().contains("fill-first"));
    let undone = ok(&ctx, Command::ConfigUndo).await;
    assert_eq!(undone.json["via"], json!("server"));
    assert_eq!(live.state.config().routing.strategy, "fill-first");

    // A key through the server.
    let added = ok(
        &ctx,
        Command::KeysAdd(AddInput {
            generate: true,
            ..AddInput::default()
        }),
    )
    .await;
    assert_eq!(added.json["via"], json!("server"));
    // The management API's own write puts the defaults into the file too,
    // and the report says what that means.
    if added.json["changes"].as_array().unwrap().len() > 1 {
        assert!(
            added.json["note"]
                .as_str()
                .unwrap()
                .contains("at their defaults")
        );
    }
    let new_key = added.json["key"].as_str().unwrap().to_owned();
    assert!(live.state.config().api_keys.contains(&new_key));
    let removed = ok(
        &confirmed(&live.setup.path, Caller::Cli),
        Command::KeysRemove(RemoveInput {
            index: Some(1),
            source: None,
        }),
    )
    .await;
    assert_eq!(removed.json["via"], json!("server"));
    assert!(!live.state.config().api_keys.contains(&new_key));

    // The whole config, replaced through the server.
    let replacement = live.setup.file(
        "replacement.yaml",
        &format!(
            "{}routing:\n  strategy: \"round-robin\"\n",
            config_text(live.port, Some(KEY), &live.setup.auth_dir)
        ),
    );
    let replace = || {
        Command::ConfigReplace(ReplaceInput {
            source: Source::File(replacement.clone()),
        })
    };
    let failure = fails(&ctx, replace()).await;
    assert_eq!(failure.error, "needs_confirmation");
    assert!(failure.message.contains("replaces the whole config"));
    let replaced = ok(&confirmed(&live.setup.path, Caller::Cli), replace()).await;
    assert_eq!(replaced.json["via"], json!("server"));
    assert_eq!(live.state.config().routing.strategy, "round-robin");
}

// Not upstream's: through the server, an undo sends the SHA-256 of the
// file and backup it worked out from: one of a hand-edited config is
// refused with changed_since unless confirmed, and one whose backup changed
// after it was asked about is refused by the server with config_changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_through_the_server_checks_what_it_saw() {
    let live = live(Some(KEY), None).await;
    let path = live.setup.path.clone();
    let backup = live.setup.dir.path().join("config.yaml.bak");
    let changed = ok(&cli(&path), set("routing.strategy", "fill-first")).await;
    assert_eq!(changed.json["via"], json!("server"));
    let edited = live.setup.text().replace("fill-first", "round-robin");
    std::fs::write(&path, &edited).unwrap();

    let failure = fails(&cli(&path), Command::ConfigUndo).await;
    assert_eq!(failure.error, "changed_since");
    assert_eq!(live.setup.text(), edited);

    let saved = std::fs::read_to_string(&backup).unwrap();
    let failure = fails(&asking_and_editing(&path, &backup), Command::ConfigUndo).await;
    assert_eq!(failure.error, "config_changed", "{failure:?}");
    assert_eq!(live.setup.text(), edited);
    std::fs::write(&backup, &saved).unwrap();

    let undone = ok(&confirmed(&path, Caller::Cli), Command::ConfigUndo).await;
    assert_eq!(undone.json["via"], json!("server"));
    assert!(!live.setup.text().contains("round-robin"));
    assert_ne!(live.state.config().routing.strategy, "fill-first");
}

// Not upstream's: a server on the config's port that takes its key but
// runs another config file is never changed or asked about: each command
// says a server at that address runs another config, and nothing changes
// in either file or in the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_running_another_config_is_left_alone() {
    let live = live(Some(KEY), None).await;
    let other = Setup::new(live.port, Some(KEY));
    std::fs::write(
        &other.path,
        format!(
            "{}routing:\n  strategy: \"fill-first\"\n",
            config_text(live.port, Some(KEY), &other.auth_dir)
        ),
    )
    .unwrap();
    // A backup, so undo has something to put back.
    other.file("config.yaml.bak", &live.setup.text());
    let ours = other.text();
    let theirs = live.setup.text();
    let strategy = live.state.config().routing.strategy.clone();
    let ctx = confirmed(&other.path, Caller::Cli);
    let refused = |failure: &Failure| {
        assert_eq!(failure.error, "other_config", "{failure:?}");
        assert_eq!(failure.code, exit::FAILED);
        assert!(failure.message.contains("runs another config"));
        assert!(
            failure
                .message
                .contains(&format!("127.0.0.1:{}", live.port))
        );
        assert!(failure.hint.as_deref().unwrap().contains("--config"));
        assert!(!failure_shows(failure, KEY));
    };

    refused(&fails(&ctx, set("routing.strategy", "round-robin")).await);
    refused(&fails(&ctx, unset("routing.strategy")).await);
    refused(&fails(&ctx, Command::ConfigUndo).await);
    refused(
        &fails(
            &ctx,
            Command::KeysAdd(AddInput {
                generate: true,
                ..AddInput::default()
            }),
        )
        .await,
    );
    refused(&fails(&ctx, Command::CredentialsList(CredentialsList::default())).await);

    let status = ok(&ctx, Command::Status).await;
    assert_eq!(status.code, exit::NOT_RUNNING);
    assert_eq!(status.json["running"], json!(false));
    assert_eq!(status.json["management"], json!("other_config"));
    assert!(
        status.json["reason"]
            .as_str()
            .unwrap()
            .contains("runs another config")
    );

    assert_eq!(other.text(), ours);
    assert_eq!(live.setup.text(), theirs);
    assert_eq!(live.state.config().routing.strategy, strategy);
    assert_eq!(live.state.config().api_keys, vec![CLIENT_KEY.to_owned()]);

    // The config the server runs is still changed through it.
    let changed = ok(
        &cli(&live.setup.path),
        set("routing.strategy", "fill-first"),
    )
    .await;
    assert_eq!(changed.json["via"], json!("server"));
    assert_eq!(other.text(), ours);
}

// Not upstream's: a change is made only to the file it was worked out
// from. One a person said yes to at the terminal is refused when the file
// changed while they were asked; one confirmed up front is worked out
// again from the file as it is.
#[tokio::test]
async fn a_change_is_made_only_to_the_file_it_was_worked_out_from() {
    let offline = offline(Some(KEY));
    let path = offline.setup.path.clone();
    let edited = format!(
        "{}routing:\n  strategy: \"fill-first\"\n",
        offline.setup.text()
    );
    let asked = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&asked);
    let (target, text) = (path.clone(), edited.clone());
    let ctx = Context {
        ask: Some(Box::new(move |_question: &str| {
            count.fetch_add(1, Ordering::SeqCst);
            std::fs::write(&target, &text).unwrap();
            true
        })),
        ..cli(&path)
    };
    let failure = fails(&ctx, set("server.host", "0.0.0.0")).await;
    assert_eq!(failure.error, "config_changed");
    assert_eq!(failure.code, exit::FAILED);
    assert!(failure.hint.as_deref().unwrap().contains("run it again"));
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_eq!(offline.setup.text(), edited);
    assert!(!offline.setup.text().contains("0.0.0.0"));

    // Confirmed up front, the change is worked out from the file as it is,
    // and keeps the edit.
    let changed = ok(
        &confirmed(&path, Caller::Cli),
        set("server.host", "0.0.0.0"),
    )
    .await;
    assert_eq!(changed.json["via"], json!("file"));
    let text = offline.setup.text();
    assert!(text.contains("0.0.0.0"));
    assert!(text.contains("fill-first"));
}

// Not upstream's: the management key is the config's plain one, else
// MANAGEMENT_PASSWORD, else the key file; with none the change goes to the
// file; a key the server refuses stops the change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_key_is_found_in_order() {
    let live = live(Some(HASHED), Some(PASSWORD)).await;
    let path = &live.setup.path;

    // No key: the server runs, the change goes to the file.
    let ctx = cli(path);
    let status = ok(&ctx, Command::Status).await;
    assert_eq!(status.json["running"], json!(true));
    assert_eq!(status.json["management"], json!("no_key"));
    let changed = ok(&ctx, set("routing.strategy", "fill-first")).await;
    assert_eq!(changed.json["via"], json!("file"));
    assert!(
        changed.json["note"]
            .as_str()
            .unwrap()
            .contains("no management key")
    );
    let failure = fails(&ctx, Command::CredentialsList(CredentialsList::default())).await;
    assert_eq!(failure.error, "no_management_key");
    assert!(failure.hint.unwrap().contains("MANAGEMENT_PASSWORD"));

    // MANAGEMENT_PASSWORD.
    let with_password = Context {
        env: Env {
            password: Some(PASSWORD.to_owned()),
            key_file: None,
        },
        ..cli(path)
    };
    let status = ok(&with_password, Command::Status).await;
    assert_eq!(status.json["key_source"], json!("MANAGEMENT_PASSWORD"));
    assert!(!shows(&status, PASSWORD));
    let changed = ok(&with_password, set("routing.strategy", "round-robin")).await;
    assert_eq!(changed.json["via"], json!("server"));

    // The key file, from the flag or the environment.
    let key_file = live.setup.file("management-key", &format!("{PASSWORD}\n"));
    for ctx in [
        Context {
            key_file: Some(key_file.clone()),
            ..cli(path)
        },
        Context {
            env: Env {
                password: None,
                key_file: Some(key_file.clone()),
            },
            ..cli(path)
        },
    ] {
        let status = ok(&ctx, Command::Status).await;
        assert_eq!(status.json["key_source"], json!("key-file"));
        assert!(!shows(&status, PASSWORD));
    }

    // A wrong key is refused, and nothing is changed.
    let wrong = Context {
        env: Env {
            password: Some("a-wrong-password-value".to_owned()),
            key_file: None,
        },
        ..cli(path)
    };
    let before = live.setup.text();
    let failure = fails(&wrong, set("routing.strategy", "fill-first")).await;
    assert_eq!(failure.error, "unauthorized");
    assert_eq!(failure.code, exit::FAILED);
    assert!(!failure_shows(&failure, "a-wrong-password-value"));
    assert_eq!(live.setup.text(), before);
    let status = ok(&wrong, Command::Status).await;
    assert_eq!(status.json["management"], json!("refused"));
}

// Not upstream's: changes through the agent commands and the management
// API's own writes, made at once, each take the server's write lock, so
// none is lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_keep_each_other() {
    let live = Arc::new(live(Some(KEY), None).await);
    let mut tasks = Vec::new();
    for (path, value) in [
        ("routing.strategy", "fill-first"),
        ("routing.session-affinity", "true"),
        ("routing.retry.max-retry-interval", "7"),
        ("routing.retry.max-retry-credentials", "2"),
        ("observability.logs.request-log", "true"),
    ] {
        let live = Arc::clone(&live);
        tasks.push(tokio::spawn(async move {
            let ctx = cli(&live.setup.path);
            let changed = ok(&ctx, set(path, value)).await;
            assert_eq!(changed.json["via"], json!("server"));
        }));
    }
    for (route, value) in [
        ("/v0/management/debug", json!(true)),
        ("/v0/management/request-retry", json!(5)),
        ("/v0/management/logging-to-file", json!(true)),
        ("/v0/management/usage-statistics-enabled", json!(true)),
    ] {
        let live = Arc::clone(&live);
        tasks.push(tokio::spawn(async move {
            live.remote(KEY)
                .json(
                    Method::PUT,
                    route,
                    Some(Body::Json(json!({"value": value}))),
                )
                .await
                .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let ctx = cli(&live.setup.path);
    for (path, value) in [
        ("routing.strategy", json!("fill-first")),
        ("routing.session-affinity", json!(true)),
        ("routing.retry.max-retry-interval", json!(7)),
        ("routing.retry.max-retry-credentials", json!(2)),
        ("observability.logs.request-log", json!(true)),
        ("observability.logs.debug", json!(true)),
        ("routing.retry.request-retry", json!(5)),
        ("observability.logs.logging-to-file", json!(true)),
        ("observability.usage.usage-statistics-enabled", json!(true)),
    ] {
        assert_eq!(ok(&ctx, get(path)).await.json["value"], value, "{path}");
    }
    let config = live.state.config();
    assert_eq!(config.routing.strategy, "fill-first");
    assert!(config.debug);
}

// Not upstream's: the running server's credentials are listed with their
// state and masked names, turned off and on, their quota reset, and
// removed with a confirmation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credentials_go_through_the_running_server() {
    let live = live(Some(KEY), None).await;
    let file = live.add_credential(
        "codex-someone@example.com.json",
        r#"{"type":"codex","email":"someone@example.com","access_token":"at-secret-token-value-123"}"#,
    );
    let ctx = cli(&live.setup.path);
    let listed = ok(&ctx, Command::CredentialsList(CredentialsList::default())).await;
    assert_eq!(listed.json["count"], json!(1));
    let entry = &listed.json["credentials"][0];
    assert_eq!(entry["provider"], json!("codex"));
    assert_eq!(entry["source"], json!("file"));
    assert_eq!(entry["state"], json!("ready"));
    assert!(!shows(&listed, "someone@example.com"));
    assert!(!shows(&listed, "at-secret-token-value-123"));
    let index = entry["auth_index"].as_str().unwrap().to_owned();
    assert!(listed.text.contains(&index));

    let filtered = ok(
        &ctx,
        Command::CredentialsList(CredentialsList {
            state: Some("off".to_owned()),
            provider: None,
        }),
    )
    .await;
    assert_eq!(filtered.json["count"], json!(0));
    let failure = fails(
        &ctx,
        Command::CredentialsList(CredentialsList {
            state: Some("sleepy".to_owned()),
            provider: None,
        }),
    )
    .await;
    assert_eq!(failure.code, exit::USAGE);

    let disabled = ok(&ctx, Command::CredentialsDisable(credential(&index))).await;
    assert_eq!(disabled.json["changed"], json!(true));
    assert!(
        disabled
            .text
            .contains(&format!("open-ferry credentials enable {index}"))
    );
    let listed = ok(&ctx, Command::CredentialsList(CredentialsList::default())).await;
    assert_eq!(listed.json["credentials"][0]["state"], json!("off"));
    let again = ok(&ctx, Command::CredentialsDisable(credential(&index))).await;
    assert_eq!(again.json["changed"], json!(false));
    let enabled = ok(&ctx, Command::CredentialsEnable(credential(&index))).await;
    assert_eq!(enabled.json["changed"], json!(true));

    let reset = ok(&ctx, Command::CredentialsResetQuota(credential(&index))).await;
    assert_eq!(reset.json["action"], json!("reset_quota"));
    assert_eq!(
        fails(&ctx, Command::CredentialsEnable(credential("nobody")))
            .await
            .error,
        "not_found"
    );

    let failure = fails(&ctx, Command::CredentialsRemove(credential(&index))).await;
    assert_eq!(failure.error, "needs_confirmation");
    assert!(file.exists());
    let removed = ok(
        &confirmed(&live.setup.path, Caller::Cli),
        Command::CredentialsRemove(credential(&index)),
    )
    .await;
    assert_eq!(removed.json["changed"], json!(true));
    assert!(!file.exists());
    let listed = ok(&ctx, Command::CredentialsList(CredentialsList::default())).await;
    assert_eq!(listed.json["count"], json!(0));
    assert_eq!(listed.text, "No credentials.\n");
}

/// The query of a sign-in status request.
#[derive(serde::Deserialize)]
struct StateQuery {
    state: String,
}

/// A server that answers the management API's sign-in routes as open-ferry
/// does, without signing in anywhere.
async fn login_server() -> (Offline, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let setup = Setup::new(port, Some(KEY));
    let config = setup.text();
    let app = Router::new()
        .route("/v0/management/debug", get_route(|| async { Json(json!({"debug": false})) }))
        .route("/v0/management/config.yaml", get_route(move || async move { config }))
        .route(
            "/v0/management/codex-auth-url",
            get_route(|| async {
                Json(json!({"status": "ok", "url": "https://auth.example.com/authorize?client=x", "state": "state-one"}))
            }),
        )
        .route(
            "/v0/management/get-auth-status",
            get_route(|Query(query): Query<StateQuery>| async move {
                Json(match query.state.as_str() {
                    "state-one" => json!({"status": "ok"}),
                    "state-bad" => json!({"status": "error", "error": "refused for someone@example.com"}),
                    _ => json!({"status": "error", "error": "unknown state"}),
                })
            }),
        );
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // The config is the server's; the socket held is any other port.
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    (
        Offline {
            setup,
            _socket: socket,
        },
        task,
    )
}

// Not upstream's: a sign-in gives the address to open and the state to
// wait on; waiting on the state says when it finished, or why it failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_gives_the_address_and_waits() {
    let (server, _task) = login_server().await;
    let path = &server.setup.path;
    let login = |state: Option<&str>| {
        Command::CredentialsLogin(LoginInput {
            provider: "codex".to_owned(),
            state: state.map(str::to_owned),
            wait: false,
        })
    };
    let started = ok(&context(path, Caller::Mcp), login(None)).await;
    assert_eq!(started.json["status"], json!("wait"));
    assert_eq!(
        started.json["url"],
        json!("https://auth.example.com/authorize?client=x")
    );
    assert_eq!(started.json["state"], json!("state-one"));
    assert!(started.text.contains("credentials_login again with state"));
    let started = ok(&cli(path), login(None)).await;
    assert!(
        started
            .text
            .contains("open-ferry credentials login codex --state state-one")
    );

    let done = ok(&context(path, Caller::Mcp), login(Some("state-one"))).await;
    assert_eq!(done.json["status"], json!("ok"));
    let failure = fails(&context(path, Caller::Mcp), login(Some("state-bad"))).await;
    assert_eq!(failure.error, "login_failed");
    assert!(!failure.message.contains("someone@example.com"));

    // A Claude sign-in goes ahead only with a confirmation.
    let claude = Command::CredentialsLogin(LoginInput {
        provider: "claude".to_owned(),
        state: None,
        wait: false,
    });
    assert_eq!(fails(&cli(path), claude).await.error, "needs_confirmation");
}

/// A client's side of an MCP session over an in-memory pipe.
struct Session {
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    next: u64,
}

impl Session {
    async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    /// Sends the request `method` and gives back its answer.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let line = tokio::time::timeout(Duration::from_secs(30), self.lines.next_line())
                .await
                .expect("no answer in time")
                .unwrap()
                .expect("the server closed the session");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message.get("id") == Some(&json!(id)) {
                return message;
            }
        }
    }

    /// Calls the tool `name` with `arguments`, and gives back its result.
    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        let answer = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        answer
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{name}: {answer}"))
    }
}

/// Starts an MCP session with `server` and initializes it with the
/// protocol version `version`, giving back the session and the answer.
async fn initialized(server: Server, version: &str) -> (Session, Value) {
    let (client, server_io) = tokio::io::duplex(1 << 20);
    let transport = tokio::io::split(server_io);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(transport).await {
            let _ = running.waiting().await;
        }
    });
    let (reader, writer) = tokio::io::split(client);
    let mut session = Session {
        lines: BufReader::new(reader).lines(),
        writer,
        next: 0,
    };
    let answer = session
        .request(
            "initialize",
            json!({
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": {"name": "open-ferry-tests", "version": "0"},
            }),
        )
        .await;
    session
        .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    (session, answer)
}

async fn server_session(server: Server) -> Session {
    initialized(server, "2025-06-18").await.0
}

// Not upstream's: the MCP server answers the handshake of the current
// protocol version and earlier ones, and lists its tools with their
// annotations and its resources.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_handshake_tools_and_resources() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    for version in ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"] {
        let server = Server::new(Ok(setup.path.clone()), Env::default(), None);
        let (mut session, answer) = initialized(server, version).await;
        let result = &answer["result"];
        assert_eq!(result["protocolVersion"], json!(version), "{answer}");
        assert_eq!(result["serverInfo"]["name"], json!("open-ferry"));
        assert!(result["capabilities"]["tools"].is_object());
        assert!(result["capabilities"]["resources"].is_object());
        assert!(
            result["instructions"]
                .as_str()
                .unwrap()
                .contains("open-ferry")
        );
        let tools = session.request("tools/list", json!({})).await;
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 18);
    }

    let server = Server::new(Ok(setup.path.clone()), Env::default(), None);
    let mut session = server_session(server).await;
    let tools = session.request("tools/list", json!({})).await;
    let tools = tools["result"]["tools"].as_array().unwrap().clone();
    let tool = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["name"] == json!(name))
            .cloned()
            .unwrap_or_else(|| panic!("no {name}"))
    };
    let hints = |name: &str| {
        let annotations = tool(name)["annotations"].clone();
        (
            annotations["readOnlyHint"].as_bool(),
            annotations["destructiveHint"].as_bool(),
            annotations["idempotentHint"].as_bool(),
            annotations["openWorldHint"].as_bool(),
        )
    };
    for name in [
        "status",
        "config_get",
        "config_show",
        "config_diff",
        "keys_list",
        "credentials_list",
        "clients_setup",
    ] {
        assert_eq!(
            hints(name),
            (Some(true), None, Some(true), Some(false)),
            "{name}"
        );
    }
    for name in [
        "config_set",
        "config_unset",
        "config_replace",
        "credentials_disable",
    ] {
        assert_eq!(
            hints(name),
            (Some(false), Some(true), Some(true), Some(false)),
            "{name}"
        );
    }
    for name in ["config_undo", "keys_remove", "credentials_remove"] {
        assert_eq!(
            hints(name),
            (Some(false), Some(true), Some(false), Some(false)),
            "{name}"
        );
    }
    assert_eq!(
        hints("keys_add"),
        (Some(false), Some(false), Some(false), Some(false))
    );
    for name in ["credentials_enable", "credentials_reset_quota"] {
        assert_eq!(
            hints(name),
            (Some(false), Some(false), Some(true), Some(false)),
            "{name}"
        );
    }
    assert_eq!(
        hints("credentials_login"),
        (Some(false), Some(false), Some(false), Some(true))
    );
    let schema = &tool("config_set")["inputSchema"];
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(schema["required"], json!(["path"]));
    assert!(schema["properties"]["confirm"].is_object());
    assert!(
        tool("config_set")["description"]
            .as_str()
            .unwrap()
            .contains("open-ferry config set")
    );

    // A call: its JSON as the structured result and the first text, then
    // its text.
    let result = session
        .call("config_get", json!({"path": "server.port"}))
        .await;
    assert_eq!(result["isError"], json!(false));
    assert_eq!(
        result["structuredContent"]["value"],
        json!(offline_port(setup))
    );
    let content = result["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    let first: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(first, result["structuredContent"]);
    assert!(
        content[1]["text"]
            .as_str()
            .unwrap()
            .starts_with("server.port: ")
    );

    // A failure is a tool error with the failure as its result.
    let result = session
        .call(
            "config_set",
            json!({"path": "server.host", "value": "0.0.0.0"}),
        )
        .await;
    assert_eq!(result["isError"], json!(true));
    assert_eq!(
        result["structuredContent"]["error"],
        json!("needs_confirmation")
    );
    assert!(
        result["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("needs confirm: true")
    );
    // An argument the tool doesn't take, and a tool that doesn't exist.
    let result = session
        .call("config_get", json!({"path": "server.port", "secret": "x"}))
        .await;
    assert_eq!(result["structuredContent"]["error"], json!("usage"));
    let answer = session
        .request("tools/call", json!({"name": "nope", "arguments": {}}))
        .await;
    assert_eq!(answer["error"]["code"], json!(-32602));

    // The resources.
    let listed = session.request("resources/list", json!({})).await;
    let uris: Vec<&str> = listed["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|resource| resource["uri"].as_str())
        .collect();
    assert_eq!(uris, [DOCS_URI, CONFIG_URI]);
    let docs = session
        .request("resources/read", json!({"uri": DOCS_URI}))
        .await;
    let docs = &docs["result"]["contents"][0];
    assert_eq!(docs["mimeType"], json!("text/markdown"));
    assert!(docs["text"].as_str().unwrap().contains("open-ferry mcp"));
    let config = session
        .request("resources/read", json!({"uri": CONFIG_URI}))
        .await;
    let config = &config["result"]["contents"][0];
    assert_eq!(config["mimeType"], json!("application/yaml"));
    assert!(!config["text"].as_str().unwrap().contains(KEY));
    let missing = session
        .request("resources/read", json!({"uri": "open-ferry://nope"}))
        .await;
    assert!(missing["error"].is_object());
}

/// The port `setup`'s config gives the server.
fn offline_port(setup: &Setup) -> u64 {
    let text = setup.text();
    text.lines()
        .find_map(|line| line.trim().strip_prefix("port: "))
        .and_then(|port| port.parse().ok())
        .unwrap()
}

// Not upstream's: each tool, called as an agent would, against a running
// server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_tool_runs() {
    let live = live(Some(KEY), None).await;
    let setup = &live.setup;
    live.add_credential(
        "codex-tool@example.com.json",
        r#"{"type":"codex","email":"tool@example.com"}"#,
    );
    let server = Server::new(Ok(setup.path.clone()), Env::default(), None);
    let mut session = server_session(server).await;
    let mut call = async |name: &str, arguments: Value| {
        let result = session.call(name, arguments).await;
        let text = result.to_string();
        assert!(!text.contains(KEY), "{name}: {text}");
        assert!(!text.contains(CLIENT_KEY), "{name}: {text}");
        result
    };
    let good = |result: &Value| result["isError"] == json!(false);

    let status = call("status", json!({})).await;
    assert!(good(&status), "{status}");
    assert_eq!(status["structuredContent"]["running"], json!(true));
    assert!(good(
        &call("config_get", json!({"path": "server.port"})).await
    ));
    let set = call(
        "config_set",
        json!({"path": "routing.strategy", "value": "fill-first"}),
    )
    .await;
    assert!(good(&set), "{set}");
    assert_eq!(set["structuredContent"]["via"], json!("server"));
    assert_eq!(
        set["structuredContent"]["undo"],
        json!("Undo it with the config_undo tool.")
    );
    assert!(good(&call("config_diff", json!({})).await));
    assert!(good(&call("config_undo", json!({})).await));
    assert!(good(
        &call("config_unset", json!({"path": "routing.strategy"})).await
    ));
    assert!(good(&call("config_show", json!({})).await));
    let replacement = setup.file(
        "replacement.yaml",
        &format!(
            "{}routing:
  strategy: \"fill-first\"
",
            config_text(live.port, Some(KEY), &setup.auth_dir)
        ),
    );
    let replace = json!({"from_file": replacement.display().to_string()});
    let refused = call("config_replace", replace.clone()).await;
    assert_eq!(
        refused["structuredContent"]["error"],
        json!("needs_confirmation")
    );
    let mut confirmed_replace = replace;
    confirmed_replace["confirm"] = json!(true);
    assert!(good(&call("config_replace", confirmed_replace).await));

    assert!(good(&call("keys_list", json!({})).await));
    let key_file = setup.dir.path().join("tool-key.txt");
    let added = call(
        "keys_add",
        json!({"generate": true, "to_file": key_file.display().to_string()}),
    )
    .await;
    assert!(good(&added), "{added}");
    let removed = call("keys_remove", json!({"index": 1, "confirm": true})).await;
    assert!(good(&removed), "{removed}");

    let listed = call("credentials_list", json!({"provider": "codex"})).await;
    assert!(good(&listed), "{listed}");
    let index = listed["structuredContent"]["credentials"][0]["auth_index"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!listed.to_string().contains("tool@example.com"));
    for name in [
        "credentials_disable",
        "credentials_enable",
        "credentials_reset_quota",
    ] {
        let result = call(name, json!({"credential": index})).await;
        assert!(good(&result), "{name}: {result}");
    }
    let removed = call(
        "credentials_remove",
        json!({"credential": index, "confirm": true}),
    )
    .await;
    assert!(good(&removed), "{removed}");
    let login = call("credentials_login", json!({"provider": "claude"})).await;
    assert_eq!(
        login["structuredContent"]["error"],
        json!("needs_confirmation")
    );
    let setup_result = call("clients_setup", json!({"client": "codex"})).await;
    assert!(good(&setup_result), "{setup_result}");
    let failure = call("clients_setup", json!({"client": "codex", "reveal": true})).await;
    assert_eq!(failure["structuredContent"]["error"], json!("usage"));
}
