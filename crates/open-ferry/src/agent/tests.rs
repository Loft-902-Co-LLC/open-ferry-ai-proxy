//! Tests of the agent commands, each against a config in a temporary
//! directory: with no server running for it (its port on 127.0.0.1 held
//! by a socket that doesn't listen), and with a test server running for
//! it (the management API's and the dashboard's routes, on an ephemeral
//! port of 127.0.0.1), and of the MCP server over an in-memory pipe. Every
//! key here is a dummy.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
        expect_sha256: None,
        expect_backup_sha256: None,
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

// Not upstream's: a config or a value that doesn't load is said not to,
// and where (a line, a setting), never in the loader's or writer's words,
// which can quote a value: a secret put where it doesn't belong never
// shows.
#[tokio::test]
async fn load_errors_say_only_where() {
    const MISPLACED: &str = "sk-misplaced-secret-0123456789abcdef";
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let before = setup.text();
    let ctx = confirmed(&setup.path, Caller::Cli);

    // A value the writer refuses, as its message quotes the entry.
    let failure = fails(
        &ctx,
        set("server.trusted-proxies", &format!(r#"["{MISPLACED}"]"#)),
    )
    .await;
    assert_eq!(failure.error, "invalid_value", "{failure:?}");
    assert!(
        failure
            .message
            .starts_with("that would make the config invalid"),
        "{failure:?}"
    );
    assert!(!failure_shows(&failure, MISPLACED));
    // A value that isn't YAML, with its line.
    let failure = fails(
        &ctx,
        set("routing.strategy", &format!("\n[{MISPLACED}, {{")),
    )
    .await;
    assert_eq!(failure.error, "usage", "{failure:?}");
    assert!(failure.message.starts_with("the value isn't YAML or JSON"));
    assert!(!failure_shows(&failure, MISPLACED));
    assert_eq!(setup.text(), before);

    // A config that doesn't load: status says so, and get and set say where.
    let broken = before.replace(
        "server:\n",
        &format!("server:\n  trusted-proxies: [\"{MISPLACED}\"]\n"),
    );
    std::fs::write(&setup.path, &broken).unwrap();
    let status = ok(&cli(&setup.path), Command::Status).await;
    assert!(
        status.text.contains("the config doesn't load"),
        "{}",
        status.text
    );
    assert!(!shows(&status, MISPLACED));
    let unloadable = format!("{before}{MISPLACED}: [\n");
    std::fs::write(&setup.path, &unloadable).unwrap();
    let status = ok(&cli(&setup.path), Command::Status).await;
    assert!(
        status
            .text
            .contains("the config doesn't load: it isn't YAML"),
        "{}",
        status.text
    );
    assert!(status.text.contains("line "), "{}", status.text);
    assert!(!shows(&status, MISPLACED));
    for command in [
        get("routing.strategy"),
        set("routing.strategy", "fill-first"),
    ] {
        let failure = fails(&ctx, command).await;
        assert_eq!(failure.error, "invalid_config", "{failure:?}");
        assert!(failure.message.contains("line "), "{failure:?}");
        assert!(!failure_shows(&failure, MISPLACED));
    }
    assert_eq!(setup.text(), unloadable);
}

// Not upstream's: where a loader's message places a problem is kept, and
// nothing else of it.
#[test]
fn a_load_message_is_cut_to_where() {
    use super::values::placed;
    assert_eq!(
        placed(
            "it doesn't load",
            "yaml: unmarshal errors:\n  line 7: field sk-abc-0123 not found in type config.Routing"
        ),
        "it doesn't load (line 7)"
    );
    assert_eq!(
        placed(
            "it doesn't load",
            "invalid trusted-proxies entry \"sk-abc-0123\" in server.trusted-proxies[2]"
        ),
        "it doesn't load (server.trusted-proxies)"
    );
    assert_eq!(
        placed(
            "it doesn't load",
            "legacy field host is not accepted by v8; use server.host"
        ),
        "it doesn't load (server.host)"
    );
    assert_eq!(
        placed("it doesn't load", "sk-abc-0123 routing"),
        "it doesn't load"
    );
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

/// Whether `command` is refused for want of a confirmation, from the
/// command line and as a tool, with a reason that holds each of `reasons`,
/// leaving the config as it was.
async fn needs_confirmation_for(setup: &Setup, command: &Command, reasons: &[&str]) {
    let before = setup.text();
    for caller in [Caller::Cli, Caller::Mcp] {
        let failure = fails(&context(&setup.path, caller), command.clone()).await;
        assert_eq!(
            failure.error, "needs_confirmation",
            "{reasons:?}: {failure:?}"
        );
        let would = failure.would.clone().unwrap();
        let given = would["reasons"].to_string();
        for reason in reasons {
            assert!(given.contains(reason), "{reason} not in {given}");
        }
        assert!(!failure_shows(&failure, KEY));
        assert!(!failure_shows(&failure, CLIENT_KEY));
        assert_eq!(setup.text(), before);
    }
}

// Not upstream's: a section set or unset whole is checked for each
// sensitive setting in it, and a parent set to a value that isn't a
// mapping is refused.
#[tokio::test]
async fn sections_set_whole_are_checked() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let before = setup.text();
    needs_confirmation_for(setup, &set("server", "{}"), &["server.host"]).await;
    needs_confirmation_for(setup, &unset("server"), &["server.host"]).await;
    needs_confirmation_for(
        setup,
        &set("server", r#"{"host": "0.0.0.0", "port": 1}"#),
        &["server.host would be 0.0.0.0"],
    )
    .await;
    needs_confirmation_for(setup, &set("management", "{}"), &["management.secret-key"]).await;
    needs_confirmation_for(setup, &unset("management"), &["management.secret-key"]).await;
    needs_confirmation_for(
        setup,
        &set("management", r#"{"allow-remote": true}"#),
        &["management.allow-remote", "management.secret-key"],
    )
    .await;
    for ctx in [cli(&setup.path), confirmed(&setup.path, Caller::Mcp)] {
        for value in ["null", "1", "[]", "text"] {
            for path in ["server", "management", "access"] {
                let failure = fails(&ctx, set(path, value)).await;
                assert_eq!(
                    failure.error, "invalid_value",
                    "{path} {value}: {failure:?}"
                );
                assert_eq!(setup.text(), before, "{path} {value}");
            }
        }
    }
}

// Not upstream's: a pre-v8 name isn't taken for its v8 setting, so it
// can't go around the checks: `config set` and `config unset` refuse it,
// naming the v8 one, and so does `config replace`, for a config that holds
// one.
#[tokio::test]
async fn legacy_names_are_refused() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let port = offline_port(setup);
    let before = setup.text();
    let ctx = confirmed(&setup.path, Caller::Cli);
    for (legacy, current, value) in [
        ("host", "server.host", "0.0.0.0"),
        ("tls.enable", "server.tls.enable", "true"),
        (
            "remote-management.allow-remote",
            "management.allow-remote",
            "true",
        ),
        ("remote-management.secret-key", "management.secret-key", ""),
    ] {
        for command in [set(legacy, value), unset(legacy)] {
            let failure = fails(&ctx, command).await;
            assert!(
                matches!(failure.error, "unknown_path" | "secret_in_argument"),
                "{legacy}: {failure:?}"
            );
            assert_eq!(failure.code, exit::USAGE);
            if failure.error == "unknown_path" {
                assert!(failure.text().contains(current), "{legacy}: {failure:?}");
            }
        }
    }
    // `api-keys` is the v8 providers' keys: never the client keys, and a
    // secret.
    let failure = fails(&ctx, set("api-keys", r#"["sk-new-0123456789"]"#)).await;
    assert_eq!(failure.error, "secret_in_argument", "{failure:?}");
    let failure = fails(
        &ctx,
        set_from(
            "api-keys",
            Source::Stdin(r#"["sk-new-0123456789"]"#.to_owned()),
        ),
    )
    .await;
    assert_eq!(failure.error, "invalid_value", "{failure:?}");
    assert!(!failure_shows(&failure, "sk-new-0123456789"));
    assert_eq!(setup.text(), before);

    // A whole config with an old name, or all in the old layout, is
    // refused, naming the v8 setting.
    let current = config_text(u16::try_from(port).unwrap(), Some(KEY), &setup.auth_dir);
    let auth_dir = setup.auth_dir.display().to_string().replace('\\', "/");
    for (legacy, name) in [
        ("host: \"0.0.0.0\"\n".to_owned(), "server.host"),
        ("tls:\n  enable: true\n".to_owned(), "server.tls"),
        (
            "remote-management:\n  allow-remote: true\n".to_owned(),
            "management.allow-remote",
        ),
        (
            format!("api-keys:\n  - \"{CLIENT_KEY}\"\n"),
            "access.api-keys",
        ),
        (
            format!(
                "host: \"0.0.0.0\"\nport: {port}\nremote-management:\n  allow-remote: true\n  secret-key: \"{KEY}\"\napi-keys: []\nauth-dir: '{auth_dir}'\n"
            ),
            "",
        ),
    ] {
        let text = if name.is_empty() {
            legacy.clone()
        } else {
            format!("{current}{legacy}")
        };
        let file = setup.file("old.yaml", &text);
        let failure = fails(
            &ctx,
            Command::ConfigReplace(ReplaceInput {
                source: Source::File(file),
            }),
        )
        .await;
        assert_eq!(failure.error, "invalid_value", "{legacy}: {failure:?}");
        assert!(failure.message.contains(name), "{legacy}: {failure:?}");
        assert!(!failure_shows(&failure, KEY));
        assert!(!failure_shows(&failure, CLIENT_KEY));
        assert_eq!(setup.text(), before);
    }
}

// Not upstream's: an undo, and a replace, give the reason of each
// sensitive setting they change, besides their own.
#[tokio::test]
async fn undo_and_replace_give_their_reasons() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let ctx = confirmed(&setup.path, Caller::Cli);
    ok(&ctx, set("server.host", "0.0.0.0")).await;
    ok(&cli(&setup.path), set("server.host", "localhost")).await;
    needs_confirmation_for(
        setup,
        &Command::ConfigUndo,
        &["server.host would be 0.0.0.0"],
    )
    .await;
    ok(&ctx, set("management.allow-remote", "true")).await;
    ok(&ctx, set("management.allow-remote", "false")).await;
    needs_confirmation_for(setup, &Command::ConfigUndo, &["management.allow-remote"]).await;
    let undone = ok(&ctx, Command::ConfigUndo).await;
    assert_eq!(
        ok(&ctx, get("management.allow-remote")).await.json["value"],
        json!(true),
        "{}",
        undone.text
    );

    let port = offline_port(setup);
    let open = setup.file(
        "open.yaml",
        &config_text(u16::try_from(port).unwrap(), Some(KEY), &setup.auth_dir).replace(
            "host: \"127.0.0.1\"",
            "host: \"192.0.2.1\"\n  trusted-proxies: [\"10.0.0.0/8\"]",
        ),
    );
    needs_confirmation_for(
        setup,
        &Command::ConfigReplace(ReplaceInput {
            source: Source::File(open),
        }),
        &[
            "it replaces the whole config",
            "server.host would be 192.0.2.1",
            "server.trusted-proxies",
            "management.allow-remote",
        ],
    )
    .await;
}

// Not upstream's: a whole config with YAML anchors, aliases and merge keys
// is checked as it loads, so a sensitive setting reached through one is
// found, and the changes name the settings, not the YAML.
#[tokio::test]
async fn anchors_and_merge_keys_are_resolved() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let port = offline_port(setup);
    let auth_dir = setup.auth_dir.display().to_string().replace('\\', "/");
    let merged = setup.file(
        "merged.yaml",
        &format!(
            "config-version: 8\nserver:\n  <<: {{host: \"0.0.0.0\"}}\n  port: {port}\nmanagement:\n  secret-key: \"{KEY}\"\naccess:\n  api-keys:\n    - &first \"{CLIENT_KEY}\"\noauth:\n  auth-dir: '{auth_dir}'\n"
        ),
    );
    let aliased = setup.file(
        "aliased.yaml",
        &format!(
            "config-version: 8\nserver:\n  host: \"127.0.0.1\"\n  port: {port}\naccess:\n  api-keys:\n    - &first \"{CLIENT_KEY}\"\nmanagement:\n  secret-key: *first\noauth:\n  auth-dir: '{auth_dir}'\n"
        ),
    );
    let replace = |file: &PathBuf| {
        Command::ConfigReplace(ReplaceInput {
            source: Source::File(file.clone()),
        })
    };
    needs_confirmation_for(setup, &replace(&merged), &["server.host would be 0.0.0.0"]).await;
    needs_confirmation_for(setup, &replace(&aliased), &["management.secret-key"]).await;
    let failure = fails(&cli(&setup.path), replace(&merged)).await;
    let changes = failure.would.unwrap()["changes"].clone();
    assert_eq!(
        changes,
        json!([{"path": "server.host", "old": "127.0.0.1", "new": "0.0.0.0"}])
    );

    let ctx = confirmed(&setup.path, Caller::Cli);
    ok(&ctx, replace(&merged)).await;
    assert_eq!(
        ok(&ctx, get("server.host")).await.json["value"],
        json!("0.0.0.0")
    );
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

/// A credential file's text, as a sign-in writes one, with the access
/// token `token`.
fn credential_text(token: &str) -> String {
    json!({
        "type": "codex",
        "email": "someone@example.com",
        "access_token": token,
        "refresh_token": "refresh-token-value-0123456789abcdef",
        "id_token": "id-token-value-0123456789abcdef"
    })
    .to_string()
}

// Not upstream's: a file is read only for a secret, never from the auth
// directory and never when it is a credential file, so a sign-in's tokens
// can't be copied into a setting that shows them; and a credential file's
// tokens are scrubbed wherever they would show.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_are_read_only_for_secrets() {
    const TOKEN: &str = "codex-access-token-abcdefghijklmnop";
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let before = setup.text();
    let credential = setup.auth_dir.join("codex-someone@example.com-plus.json");
    std::fs::write(&credential, credential_text(TOKEN)).unwrap();
    let nearby = setup.file("codex-copy.json", &credential_text(TOKEN));
    let nested = setup.file(
        "gemini-copy.json",
        &json!({"type": "gemini", "token": {"refresh_token": TOKEN}}).to_string(),
    );
    let in_auth = setup.auth_dir.join("plain.txt");
    std::fs::write(&in_auth, "a-new-management-key-value\n").unwrap();
    let plain = setup.file("repository.txt", "someone/panel\n");

    for caller in [Caller::Cli, Caller::Mcp] {
        let ctx = Context {
            yes: true,
            ..context(&setup.path, caller)
        };
        // Not for a setting that holds no secret, whatever the file.
        for file in [&plain, &nearby] {
            for string in [false, true] {
                let failure = fails(
                    &ctx,
                    Command::ConfigSet(SetInput {
                        path: "management.panel-github-repository".to_owned(),
                        value: Source::File(file.clone()),
                        string,
                    }),
                )
                .await;
                if file == &plain {
                    assert_eq!(failure.error, "usage", "{failure:?}");
                    assert_eq!(failure.code, exit::USAGE);
                    assert!(failure.message.contains("doesn't hold a secret"));
                } else {
                    assert_eq!(failure.error, "unsafe_file", "{failure:?}");
                }
                assert!(!failure_shows(&failure, TOKEN));
            }
        }
        // Not from the auth directory, nor a credential file anywhere, even
        // for a secret.
        for file in [&credential, &in_auth, &nearby, &nested] {
            let failure = fails(
                &ctx,
                set_from("management.secret-key", Source::File(file.clone())),
            )
            .await;
            assert_eq!(failure.error, "unsafe_file", "{failure:?}");
            assert_eq!(failure.code, exit::FAILED);
            assert!(
                failure
                    .hint
                    .as_deref()
                    .unwrap()
                    .contains("a file of its own")
            );
            assert!(!failure_shows(&failure, TOKEN));
            let failure = fails(
                &ctx,
                Command::KeysAdd(AddInput {
                    source: Some(Source::File(file.clone())),
                    ..AddInput::default()
                }),
            )
            .await;
            assert_eq!(failure.error, "unsafe_file");
            let failure = fails(
                &ctx,
                Command::ConfigReplace(ReplaceInput {
                    source: Source::File(file.clone()),
                }),
            )
            .await;
            assert_eq!(failure.error, "unsafe_file");
        }
        assert!(
            fails(
                &ctx,
                set_from(
                    "management.secret-key",
                    Source::File(setup.auth_dir.join("missing.txt"))
                )
            )
            .await
            .message
            .contains("can't read")
        );
        assert_eq!(setup.text(), before);
    }

    // Over MCP, `string` with `from_file` is refused for a setting that
    // holds no secret, and a credential file is refused.
    let server = Server::new(Ok(setup.path.clone()), Env::default(), None);
    let mut session = server_session(server).await;
    let result = session
        .call(
            "config_set",
            json!({"path": "management.panel-github-repository", "from_file": plain, "string": true}),
        )
        .await;
    assert_eq!(result["isError"], json!(true));
    assert_eq!(result["structuredContent"]["error"], json!("usage"));
    assert!(
        result["structuredContent"]["hint"]
            .as_str()
            .unwrap()
            .contains("`value`")
    );
    let result = session
        .call(
            "config_set",
            json!({"path": "management.panel-github-repository", "from_file": credential, "string": true}),
        )
        .await;
    assert_eq!(result["structuredContent"]["error"], json!("unsafe_file"));
    assert!(!result.to_string().contains(TOKEN));
    assert_eq!(setup.text(), before);

    // A secret's own file is read, with `string` too.
    let secret = setup.file("secret.txt", "a-new-management-key-value\n");
    let result = session
        .call(
            "config_set",
            json!({"path": "management.secret-key", "from_file": secret, "string": true, "confirm": true}),
        )
        .await;
    assert_ne!(result["isError"], json!(true), "{result}");
    assert!(!result.to_string().contains("a-new-management-key-value"));
    assert!(setup.text().contains("a-new-management-key-value"));

    // A credential file's token is scrubbed wherever it would show, even
    // in a setting that doesn't name a secret.
    let ctx = cli(&setup.path);
    let changed = ok(&ctx, set("management.panel-github-repository", TOKEN)).await;
    assert!(!shows(&changed, TOKEN), "{}", changed.text);
    assert!(changed.text.contains("[redacted]"));
    let got = ok(&ctx, get("management.panel-github-repository")).await;
    assert!(!shows(&got, TOKEN));
}

// Not upstream's: a credential file is refused whatever its shape: a
// sign-in's or a key's field at any depth to the limit, in any case and
// with any separators, or a PEM block; the refusal names the field, never
// a value. A file nested deeper than the limit is refused as too deep to
// check.
#[tokio::test]
async fn credential_files_are_refused_in_any_shape() {
    const VALUE: &str = "placeholder-credential-value-0123456789";
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let before = setup.text();
    for (name, text, mark) in [
        (
            "claude.json",
            json!({"claudeAiOauth": {"accessToken": VALUE, "refreshToken": VALUE}}).to_string(),
            "the field accessToken",
        ),
        (
            "auth.json",
            json!({"OPENAI_API_KEY": null, "tokens": {"id_token": VALUE}}).to_string(),
            "the field tokens",
        ),
        (
            "service-account.json",
            json!({"project": {"keys": [{"private_key": VALUE}]}}).to_string(),
            "the field private_key",
        ),
        (
            "session.json",
            json!({"sessionKey": VALUE}).to_string(),
            "the field sessionKey",
        ),
        (
            "oauth.yaml",
            format!("client:\n  client-secret: {VALUE}\n"),
            "the field client-secret",
        ),
        (
            "key.pem",
            format!("-----BEGIN PRIVATE KEY-----\n{VALUE}\n-----END PRIVATE KEY-----\n"),
            "a PEM block",
        ),
        // Nested deeper than it is checked to, with a field or without,
        // and deeper than JSON is read to.
        (
            "deep.json",
            format!("{}\"{VALUE}\"{}", "{\"a\": ".repeat(40), "}".repeat(40)),
            "too deeply nested to check",
        ),
        (
            "deeper.json",
            format!(
                "{}{{\"access_token\": \"{VALUE}\"}}{}",
                "[".repeat(300),
                "]".repeat(300)
            ),
            "too deeply nested to check",
        ),
    ] {
        let file = setup.file(name, &text);
        for caller in [Caller::Cli, Caller::Mcp] {
            let ctx = confirmed(&setup.path, caller);
            let failure = fails(
                &ctx,
                set_from("management.secret-key", Source::File(file.clone())),
            )
            .await;
            assert_eq!(failure.error, "unsafe_file", "{failure:?}");
            assert!(failure.message.contains(mark), "{failure:?}");
            assert!(!failure_shows(&failure, VALUE), "{failure:?}");
        }
    }
    assert_eq!(setup.text(), before);
}

// Not upstream's: a YAML file with a mapping key that isn't text (a
// number, say) can't be checked, as such a mapping has no JSON form: one
// with a credential's field in it, or nested too deeply to check below it,
// is refused, for a setting and as a whole config, and neither the config
// nor its backup changes.
#[tokio::test]
async fn a_file_with_a_key_that_isnt_text_is_refused() {
    const VALUE: &str = "placeholder-credential-value-0123456789";
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    ok(&cli(&setup.path), set("routing.strategy", "fill-first")).await;
    let backup = setup.dir.path().join("config.yaml.bak");
    let (before, backup_before) = (setup.text(), std::fs::read(&backup).unwrap());
    let deep = format!("{}\"{VALUE}\"{}", "[".repeat(40), "]".repeat(40));
    for (name, text) in [
        (
            "mixed-config.yaml",
            format!("{before}payload:\n  7: seven\n  refresh_token: {VALUE}\n"),
        ),
        (
            "mixed.yaml",
            format!("1: one\nclaudeAiOauth:\n  accessToken: {VALUE}\n"),
        ),
        ("mixed-deep.yaml", format!("1: one\nnested: {deep}\n")),
        (
            "mixed-deep-config.yaml",
            format!("{before}payload: {{1: one, nested: {deep}}}\n"),
        ),
    ] {
        let file = setup.file(name, &text);
        for caller in [Caller::Cli, Caller::Mcp] {
            let ctx = confirmed(&setup.path, caller);
            for command in [
                Command::ConfigReplace(ReplaceInput {
                    source: Source::File(file.clone()),
                }),
                set_from("management.secret-key", Source::File(file.clone())),
            ] {
                let failure = match perform(&ctx, command).await {
                    Ok(outcome) => panic!("{name} was read: {}", outcome.json),
                    Err(failure) => failure,
                };
                assert_eq!(failure.error, "unsafe_file", "{name}: {failure:?}");
                assert!(
                    failure.message.contains("a mapping key that isn't text"),
                    "{name}: {failure:?}"
                );
                assert!(!failure_shows(&failure, VALUE), "{failure:?}");
                assert_eq!(setup.text(), before, "{name}");
                assert_eq!(std::fs::read(&backup).unwrap(), backup_before, "{name}");
            }
        }
    }
}

// Not upstream's: over MCP, a call that reads a file into the config needs
// confirm: true, with that reason, even when what it changes needs none; on
// the command line, naming the file is the user's own doing.
#[tokio::test]
async fn a_file_read_over_mcp_needs_a_confirmation() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let provider = json!([{"name": "example", "base-url": "https://api.example.com", "keys": [{"api-key": "sk-provider-secret-value-1234"}]}]);
    let provider_file = setup.file("provider.json", &provider.to_string());
    let key_file = setup.file("client-key.txt", "sk-another-client-key-123456\n");
    let whole = setup.file(
        "whole.yaml",
        &format!("{}routing:\n  strategy: fill-first\n", setup.text()),
    );
    let before = setup.text();
    let server = Server::new(Ok(setup.path.clone()), Env::default(), None);
    let mut session = server_session(server).await;
    for (tool, call) in [
        (
            "config_set",
            json!({"path": "api-keys.codex", "from_file": provider_file}),
        ),
        ("keys_add", json!({"from_file": key_file})),
        ("config_replace", json!({"from_file": whole})),
    ] {
        let result = session.call(tool, call).await;
        let answer = &result["structuredContent"];
        assert_eq!(
            answer["error"],
            json!("needs_confirmation"),
            "{tool}: {answer}"
        );
        assert!(
            answer["would"]["reasons"]
                .as_array()
                .unwrap()
                .contains(&json!("it reads a file into the config")),
            "{tool}: {answer}"
        );
        assert!(!result.to_string().contains("sk-provider-secret-value-1234"));
        assert!(!result.to_string().contains("sk-another-client-key-123456"));
        assert_eq!(setup.text(), before);
    }

    // With it, they are made.
    for (tool, call) in [
        (
            "config_set",
            json!({"path": "api-keys.codex", "from_file": provider_file, "confirm": true}),
        ),
        ("keys_add", json!({"from_file": key_file, "confirm": true})),
    ] {
        let result = session.call(tool, call).await;
        assert_eq!(
            result["structuredContent"]["changed"],
            json!(true),
            "{tool}: {result}"
        );
    }
    assert!(setup.text().contains("sk-provider-secret-value-1234"));
    assert!(setup.text().contains("sk-another-client-key-123456"));

    // On the command line, a file needs no confirmation of its own.
    let other = setup.file("other-key.txt", "sk-third-client-key-1234567\n");
    let added = ok(
        &cli(&setup.path),
        Command::KeysAdd(AddInput {
            source: Some(Source::File(other)),
            ..AddInput::default()
        }),
    )
    .await;
    assert_eq!(added.json["changed"], json!(true));
}

// Not upstream's: a value read from a file or standard input shows as one
// marker wherever a change shows it, nothing of it, not even its keys: in
// what it changed, at the terminal's question, and in what it would change.
#[tokio::test]
async fn values_from_a_file_are_masked_whole() {
    const SHOWN: [&str; 5] = [
        "placeholder-provider-name",
        "api.example.com",
        "sk-provider-secret-value-1234",
        "placeholder-panel",
        "placeholder-management-key-123",
    ];
    let shows_any = |text: &str| SHOWN.iter().any(|shown| text.contains(shown));
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let provider = json!([{"name": "placeholder-provider-name", "base-url": "https://api.example.com", "keys": [{"api-key": "sk-provider-secret-value-1234"}]}]);
    let provider_file = setup.file("provider.json", &provider.to_string());

    // What it changed.
    let changed = ok(
        &cli(&setup.path),
        set_from("api-keys.codex", Source::File(provider_file)),
    )
    .await;
    assert!(!shows_any(&changed.text), "{}", changed.text);
    assert!(!shows_any(&changed.json.to_string()), "{}", changed.json);
    assert!(
        changed.json["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["path"] == json!("api-keys.codex")
                && change["new"] == json!("[redacted]")),
        "{}",
        changed.json
    );
    assert!(
        changed
            .text
            .contains("api-keys.codex: (not set) -> \"[redacted]\""),
        "{}",
        changed.text
    );
    assert!(setup.text().contains("placeholder-provider-name"));

    // The question at the terminal, for a mapping from standard input
    // with one field that names a secret.
    let management = json!({"allow-remote": false, "secret-key": "placeholder-management-key-123", "panel-github-repository": "someone/placeholder-panel"}).to_string();
    let before = setup.text();
    let question = Arc::new(std::sync::Mutex::new(String::new()));
    let seen = Arc::clone(&question);
    let ctx = Context {
        ask: Some(Box::new(move |text: &str| {
            *seen.lock().unwrap() = text.to_owned();
            false
        })),
        ..cli(&setup.path)
    };
    let failure = fails(
        &ctx,
        set_from("management", Source::Stdin(management.clone())),
    )
    .await;
    assert_eq!(failure.error, "declined");
    let asked = question.lock().unwrap().clone();
    assert!(asked.contains("management"), "{asked}");
    assert!(asked.contains("\"[redacted]\""), "{asked}");
    assert!(!shows_any(&asked), "{asked}");
    assert_eq!(setup.text(), before);

    // What it would change, without a terminal and over MCP.
    let file = setup.file("management.json", &management);
    let failure = fails(
        &cli(&setup.path),
        set_from("management", Source::File(file.clone())),
    )
    .await;
    assert_eq!(failure.error, "needs_confirmation");
    assert!(!failure_shows(&failure, "placeholder-panel"), "{failure:?}");
    assert!(!failure_shows(&failure, "placeholder-management-key-123"));
    let changes = failure.would.as_ref().unwrap()["changes"].clone();
    assert_eq!(changes.as_array().unwrap().len(), 1, "{changes}");
    assert_eq!(changes[0]["path"], json!("management"));
    assert_eq!(changes[0]["new"], json!("[redacted]"));
    let server = Server::new(Ok(setup.path.clone()), Env::default(), None);
    let mut session = server_session(server).await;
    let result = session
        .call(
            "config_set",
            json!({"path": "management", "from_file": file}),
        )
        .await;
    assert_eq!(
        result["structuredContent"]["error"],
        json!("needs_confirmation")
    );
    assert!(!shows_any(&result.to_string()), "{result}");
    assert_eq!(setup.text(), before);

    // A secret as a mapping's key, a header's name, and as a value: the
    // value shows as one marker, with no keys, shape or length, in what it
    // would change over MCP.
    const HEADER: &str = "x-placeholder-secret-header-0123";
    const HEADER_VALUE: &str = "placeholder-header-value-4567";
    const API_KEY: &str = "sk-placeholder-api-key-89ab";
    let headed = json!([{"name": "headed", "base-url": "https://api.example.com", "keys": [{"api-key": API_KEY, "headers": {HEADER: HEADER_VALUE}}]}]);
    let headed_file = setup.file("headed.json", &headed.to_string());
    let result = session
        .call(
            "config_set",
            json!({"path": "api-keys.claude", "from_file": headed_file}),
        )
        .await;
    let answer = &result["structuredContent"];
    assert_eq!(answer["error"], json!("needs_confirmation"), "{answer}");
    assert_eq!(
        answer["would"]["changes"],
        json!([{"path": "api-keys.claude", "new": "[redacted]"}]),
        "{answer}"
    );
    let text = result.to_string();
    for shown in [HEADER, HEADER_VALUE, API_KEY, "api.example.com", "headed"] {
        assert!(!text.contains(shown), "{shown}: {text}");
    }
    assert_eq!(setup.text(), before);
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

// Not upstream's: the question at the terminal, and what a change would
// do, are scrubbed of every secret of the configs it goes from and to, so
// a key that shows where no key names it, as in a URL's path, doesn't
// show: one removed from the config, and one only the new config holds.
#[tokio::test]
async fn the_question_is_scrubbed() {
    const OLD: &str = "sk-placeholder-path-key-0123456789";
    const NEW: &str = "sk-placeholder-new-path-key-456789";
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let provider = |key: &str| {
        format!(
            "api-keys:\n  codex:\n    - name: \"placeholder\"\n      base-url: \"https://api.example.com/v1/{key}/\"\n      keys:\n        - api-key: \"{key}\"\n"
        )
    };
    let base = setup.text();
    std::fs::write(&setup.path, format!("{base}{}", provider(OLD))).unwrap();
    let before = setup.text();
    let replacement = setup.file("replacement.yaml", &format!("{base}{}", provider(NEW)));
    let replace = || {
        Command::ConfigReplace(ReplaceInput {
            source: Source::File(replacement.clone()),
        })
    };

    let question = Arc::new(std::sync::Mutex::new(String::new()));
    let seen = Arc::clone(&question);
    let ctx = Context {
        ask: Some(Box::new(move |text: &str| {
            *seen.lock().unwrap() = text.to_owned();
            false
        })),
        ..cli(&setup.path)
    };
    assert_eq!(fails(&ctx, replace()).await.error, "declined");
    let asked = question.lock().unwrap().clone();
    assert!(asked.contains("api-keys.codex"), "{asked}");
    assert!(asked.contains("https://api.example.com/v1/"), "{asked}");
    for key in [OLD, NEW] {
        assert!(!asked.contains(key), "{key}: {asked}");
    }

    // Without a terminal, and over MCP, what it would change.
    let failure = fails(&cli(&setup.path), replace()).await;
    assert_eq!(failure.error, "needs_confirmation");
    for key in [OLD, NEW] {
        assert!(!failure_shows(&failure, key), "{key}: {failure:?}");
    }
    let server = Server::new(Ok(setup.path.clone()), Env::default(), None);
    let mut session = server_session(server).await;
    let result = session
        .call("config_replace", json!({"from_file": replacement}))
        .await;
    assert_eq!(
        result["structuredContent"]["error"],
        json!("needs_confirmation"),
        "{result}"
    );
    for key in [OLD, NEW] {
        assert!(!result.to_string().contains(key), "{key}: {result}");
    }
    assert_eq!(setup.text(), before);
}

// Not upstream's: a secret shorter than eight characters, a string, a
// number or a boolean, is scrubbed where it shows as a whole word, as in a
// URL's path or in free text: from the question at the terminal, the JSON
// on the command line and a tool's answer, which stay valid JSON with
// their numbers and booleans as they were. A word it is only part of is
// left as it is.
#[tokio::test]
async fn short_secrets_are_scrubbed_as_whole_words() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let provider = |path: &str, name: &str| {
        format!(
            "api-keys:\n  codex:\n    - name: \"{name}\"\n      base-url: \"https://api.example.com/v1/{path}/\"\n      keys:\n        - api-key: \"k3y9\"\n"
        )
    };
    let base = setup.text().replace(
        &format!("    - \"{CLIENT_KEY}\"\n"),
        &format!("    - \"{CLIENT_KEY}\"\n    - 4242\n    - true\n"),
    );
    let words = "k3y9 and 4242 and true, not k3y9s, 42424 or untrue";
    std::fs::write(
        &setup.path,
        format!("{base}{}", provider("k3y9/4242/true", "placeholder")),
    )
    .unwrap();
    let before = setup.text();
    let replacement = setup.file(
        "replacement.yaml",
        &format!("{base}{}", provider("true/k3y9/4242", words)),
    );
    let replace = || {
        Command::ConfigReplace(ReplaceInput {
            source: Source::File(replacement.clone()),
        })
    };
    // What shows, the paths and the words as they were and as they would
    // be, scrubbed.
    let hidden = |text: &str| {
        for shown in [
            "/k3y9/", "/4242/", "/true/", "k3y9 and", "and 4242", "and true",
        ] {
            assert!(!text.contains(shown), "{shown}: {text}");
        }
    };
    let check = |text: &str| {
        hidden(text);
        for kept in [
            "/v1/[redacted]/[redacted]/[redacted]/",
            "[redacted] and [redacted] and [redacted], not k3y9s, 42424 or untrue",
        ] {
            assert!(text.contains(kept), "{kept}: {text}");
        }
    };

    let question = Arc::new(std::sync::Mutex::new(String::new()));
    let seen = Arc::clone(&question);
    let ctx = Context {
        ask: Some(Box::new(move |text: &str| {
            *seen.lock().unwrap() = text.to_owned();
            false
        })),
        ..cli(&setup.path)
    };
    assert_eq!(fails(&ctx, replace()).await.error, "declined");
    check(&question.lock().unwrap());

    // The JSON on the command line, without a terminal.
    let failure = fails(&cli(&setup.path), replace()).await;
    assert_eq!(failure.error, "needs_confirmation");
    let printed = serde_json::to_string(&failure).unwrap();
    serde_json::from_str::<Value>(&printed).unwrap();
    check(&printed);
    let shown = ok(&cli(&setup.path), Command::ConfigShow).await;
    let printed = shown.json.to_string();
    assert_eq!(serde_json::from_str::<Value>(&printed).unwrap(), shown.json);
    assert!(
        printed.contains("/v1/[redacted]/[redacted]/[redacted]/"),
        "{printed}"
    );
    assert!(!printed.contains("k3y9/"), "{printed}");
    assert_eq!(
        shown.json["settings"]["server"]["port"],
        json!(offline_port(setup))
    );

    // A tool's answer.
    let server = Server::new(Ok(setup.path.clone()), Env::default(), None);
    let mut session = server_session(server).await;
    let result = session
        .call("config_replace", json!({"from_file": replacement}))
        .await;
    assert_eq!(
        result["structuredContent"]["error"],
        json!("needs_confirmation"),
        "{result}"
    );
    check(&result["structuredContent"].to_string());
    for content in result["content"].as_array().unwrap() {
        if let Some(text) = content["text"].as_str() {
            hidden(text);
        }
    }
    assert_eq!(setup.text(), before);
}

// Not upstream's: secrets that are words of the answers' field names, as
// `value`, `path`, `error`, `changes`, `sha256` and `true` can be, leave the
// field names as they are, in the JSON on the command line and in a tool's
// answer, which stay valid JSON, and the hint still gives the SHA-256 to go
// ahead with. The keys of a setting's value are scrubbed, and two scrubbed
// into the same one are both kept.
#[tokio::test]
async fn short_secrets_leave_the_field_names_alone() {
    const WORDS: [&str; 6] = ["value", "path", "error", "changes", "sha256", "true"];
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let keys: String = WORDS
        .iter()
        .map(|word| format!("    - '{word}'\n"))
        .collect();
    let text = setup
        .text()
        .replacen("  api-keys:\n", &format!("  api-keys:\n{keys}"), 1);
    std::fs::write(
        &setup.path,
        format!(
            "{text}routing:\n  strategy: fill-first\napi-keys:\n  codex:\n    - name: 'placeholder'\n      base-url: 'https://api.example.com/v1'\n      keys:\n        - api-key: 'sk-test-provider-key-0123456789'\n          headers:\n            value: 'a'\n            path: 'b'\n            X-Plain: 'c'\n"
        ),
    )
    .unwrap();
    let before = setup.text();
    let sha256 = |text: &str| open_ferry_core::config::save::sha256_hex(text.as_bytes());
    let shown = sha256(&before);
    let fields = |value: &Value| -> Vec<String> {
        let mut fields: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        fields.sort();
        fields
    };
    let valid = |value: &Value| {
        let printed = serde_json::to_string_pretty(value).unwrap();
        assert_eq!(&serde_json::from_str::<Value>(&printed).unwrap(), value);
    };
    let printed = |failure: &Failure| -> Value {
        serde_json::from_str(&serde_json::to_string(failure).unwrap()).unwrap()
    };

    // The JSON on the command line: a setting.
    let got = ok(&cli(&setup.path), get("routing.strategy")).await;
    valid(&got.json);
    assert_eq!(fields(&got.json), ["path", "set", "value"], "{}", got.json);
    assert_eq!(got.json["path"], json!("routing.strategy"));
    assert_eq!(got.json["value"], json!("fill-first"));

    // A mapping in a setting: its keys scrubbed, and all kept.
    let got = ok(&cli(&setup.path), get("api-keys")).await;
    valid(&got.json);
    let headers = &got.json["value"]["codex"][0]["keys"][0]["headers"];
    assert_eq!(
        fields(headers),
        ["X-Plain", "[redacted]", "[redacted] (2)"],
        "{headers}"
    );
    let mut values: Vec<&str> = headers
        .as_object()
        .unwrap()
        .values()
        .map(|value| value.as_str().unwrap())
        .collect();
    values.sort_unstable();
    assert_eq!(values, ["a", "b", "c"]);

    // A change that needs a confirmation: its fields, and the hash in the
    // hint.
    let allow = || set("management.allow-remote", "true");
    let failure = fails(&cli(&setup.path), allow()).await;
    assert_eq!(failure.error, "needs_confirmation");
    let answer = printed(&failure);
    assert_eq!(fields(&answer), ["error", "hint", "message", "would"]);
    let would = &answer["would"];
    assert_eq!(
        fields(would),
        ["changes", "config_sha256", "reasons"],
        "{would}"
    );
    assert_eq!(would["config_sha256"], json!(shown));
    assert_eq!(fields(&would["changes"][0]), ["new", "path"], "{would}");
    assert_eq!(would["changes"][0]["new"], json!(true));
    let hint = answer["hint"].as_str().unwrap();
    assert!(
        hint.contains(&format!("--yes --expect-sha256 {shown}")),
        "{hint}"
    );
    assert!(failure.text().contains("It would change:"), "{answer}");

    // Made with it.
    let ctx = Context {
        yes: true,
        expect_sha256: Some(shown.clone()),
        ..cli(&setup.path)
    };
    let changed = ok(&ctx, allow()).await;
    valid(&changed.json);
    assert_eq!(
        fields(&changed.json),
        [
            "action", "changed", "changes", "note", "path", "undo", "via"
        ],
        "{}",
        changed.json
    );
    assert_eq!(
        changed.json["changes"][0]["path"],
        json!("management.allow-remote")
    );
    assert_eq!(changed.json["changes"][0]["new"], json!(true));

    // An undo over a hand edit: both hashes in the hint.
    let edited = format!("{before}# edited by hand\n");
    std::fs::write(&setup.path, &edited).unwrap();
    let failure = fails(&cli(&setup.path), Command::ConfigUndo).await;
    assert_eq!(failure.error, "changed_since");
    let answer = printed(&failure);
    assert_eq!(
        fields(&answer["would"]),
        ["backup_sha256", "changes", "config_sha256", "reasons"],
        "{answer}"
    );
    let hint = answer["hint"].as_str().unwrap();
    let both = format!(
        "--yes --expect-sha256 {} --expect-backup-sha256 {shown}",
        sha256(&edited)
    );
    assert!(hint.contains(&both), "{hint}");

    // A tool's answers.
    let server = Server::new(Ok(setup.path.clone()), Env::default(), None);
    let mut session = server_session(server).await;
    let texts = |result: &Value| -> Vec<String> {
        result["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|content| content["text"].as_str().map(str::to_owned))
            .collect()
    };
    let result = session
        .call("config_get", json!({"path": "routing.strategy"}))
        .await;
    let answer = &result["structuredContent"];
    assert_eq!(fields(answer), ["path", "set", "value"], "{result}");
    assert_eq!(answer["value"], json!("fill-first"));
    assert_eq!(
        &serde_json::from_str::<Value>(&texts(&result)[0]).unwrap(),
        answer
    );
    let result = session
        .call(
            "config_set",
            json!({"path": "management.allow-remote", "value": true}),
        )
        .await;
    let answer = &result["structuredContent"];
    assert_eq!(answer["error"], json!("needs_confirmation"), "{result}");
    assert_eq!(
        fields(&answer["would"]),
        ["changes", "config_sha256", "reasons"],
        "{result}"
    );
    let now = sha256(&edited);
    assert_eq!(answer["would"]["config_sha256"], json!(now));
    let hint = answer["hint"].as_str().unwrap();
    assert!(
        hint.contains(&format!("confirm: true and expect_sha256: {now:?}")),
        "{hint}"
    );
    let texts = texts(&result);
    assert_eq!(&serde_json::from_str::<Value>(&texts[0]).unwrap(), answer);
    assert!(
        texts.iter().any(|text| text.contains("It would change:")),
        "{result}"
    );
    let result = session.call("config_undo", json!({})).await;
    let answer = &result["structuredContent"];
    assert_eq!(answer["error"], json!("changed_since"), "{result}");
    let hint = answer["hint"].as_str().unwrap();
    assert!(
        hint.contains(&format!(
            "confirm: true, expect_sha256: {now:?} and expect_backup_sha256: {shown:?}"
        )),
        "{hint}"
    );
    let result = session.call("keys_add", json!({"generate": true})).await;
    let hint = result["structuredContent"]["hint"].as_str().unwrap();
    assert!(
        hint.contains(&format!("confirm: true and expect_sha256: {now:?}")),
        "{result}"
    );
    assert_eq!(setup.text(), edited);
}

// Not upstream's: a number or a boolean under a secret's key, which the
// loader reads as a string key, is masked as a string is, by get, show and
// the config resource, and isn't taken inline; a switch elsewhere shows.
#[tokio::test]
async fn numbers_and_booleans_under_secrets_are_masked() {
    const NUMBER: &str = "1234567890123";
    const SECRET_NUMBER: &str = "98765432109";
    let offline = offline(None);
    let setup = &offline.setup;
    let text = setup
        .text()
        .replace(
            &format!("    - \"{CLIENT_KEY}\"\n"),
            &format!("    - true\n    - {NUMBER}\n"),
        )
        .replace(
            "access:\n",
            &format!(
                "management:\n  secret-key: {SECRET_NUMBER}\n  allow-remote: false\naccess:\n"
            ),
        );
    std::fs::write(&setup.path, &text).unwrap();
    let keys = json!(["...", "...23"]);
    let ctx = cli(&setup.path);

    let got = ok(&ctx, get("access.api-keys")).await;
    assert_eq!(got.json["value"], keys, "{}", got.json);
    assert!(
        !got.text.contains("true") && !got.text.contains(NUMBER),
        "{}",
        got.text
    );
    let secret = ok(&ctx, get("management.secret-key")).await;
    assert_eq!(secret.json["value"], json!("...09"));
    assert!(!shows(&secret, SECRET_NUMBER));
    let shown = ok(&ctx, Command::ConfigShow).await;
    assert_eq!(shown.json["settings"]["access"]["api-keys"], keys);
    assert_eq!(
        shown.json["settings"]["management"]["allow-remote"],
        json!(false)
    );
    for hidden in ["- true", NUMBER, SECRET_NUMBER] {
        assert!(!shown.text.contains(hidden), "{hidden}: {}", shown.text);
    }

    // The config resource, and the tools.
    let server = Server::new(Ok(setup.path.clone()), Env::default(), None);
    let mut session = server_session(server.clone()).await;
    let read = session
        .request("resources/read", json!({"uri": CONFIG_URI}))
        .await;
    let resource = read["result"]["contents"][0]["text"].as_str().unwrap();
    assert!(resource.contains("allow-remote: false"), "{resource}");
    for hidden in ["- true", NUMBER, SECRET_NUMBER] {
        assert!(!resource.contains(hidden), "{hidden}: {resource}");
    }
    let result = session
        .call("config_get", json!({"path": "access.api-keys"}))
        .await;
    assert_eq!(result["structuredContent"]["value"], keys, "{result}");

    // Given inline, either is a secret, and refused.
    for value in ["[true]", "[12345]"] {
        let failure = fails(
            &confirmed(&setup.path, Caller::Cli),
            set("access.api-keys", value),
        )
        .await;
        assert_eq!(failure.error, "secret_in_argument", "{failure:?}");
    }
    assert_eq!(setup.text(), text);
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
    let (config, config_sha256) = Config::load_with_sha256(&setup.path).unwrap();
    let config = Arc::new(config);
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
    .with_config_sha256(config_sha256)
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

    // A backup the same as the config: nothing to undo, and no call, so
    // no note that the server runs another file.
    let same = live.setup.text();
    std::fs::write(&backup, &same).unwrap();
    for ctx in [cli(&path), confirmed(&path, Caller::Mcp)] {
        let undone = ok(&ctx, Command::ConfigUndo).await;
        assert_eq!(undone.json["changed"], json!(false), "{}", undone.json);
        assert!(undone.json.get("via").is_none(), "{}", undone.json);
        assert!(undone.json.get("note").is_none(), "{}", undone.json);
        assert_eq!(
            undone.text,
            "Nothing to undo: the backup is the same as the config.\n"
        );
    }
    assert_eq!(live.setup.text(), same);
}

// Not upstream's: an undo's confirmation is tied to the backup it showed
// as well as to the config. Another write can change the backup and leave
// the config as it was; an undo confirmed with both hashes then puts back
// nothing (`config_changed`), in the file and through the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_is_tied_to_the_backup_it_showed() {
    let sha256 = |text: &str| open_ferry_core::config::save::sha256_hex(text.as_bytes());
    let live = live(Some(KEY), None).await;
    let offline = offline(Some(KEY));
    for (setup, via) in [(&live.setup, "server"), (&offline.setup, "file")] {
        let path = &setup.path;
        let backup = setup.dir.path().join("config.yaml.bak");
        let changed = ok(
            &confirmed(path, Caller::Cli),
            set("server.trusted-proxies", "[\"10.0.0.1\"]"),
        )
        .await;
        assert_eq!(changed.json["via"], json!(via));
        let current = setup.text();
        let shown = std::fs::read_to_string(&backup).unwrap();

        // The preview gives both hashes, and says to give both back.
        for caller in [Caller::Cli, Caller::Mcp] {
            let failure = fails(&context(path, caller), Command::ConfigUndo).await;
            assert_eq!(failure.error, "needs_confirmation", "{failure:?}");
            let would = failure.would.as_ref().unwrap();
            assert_eq!(would["config_sha256"], json!(sha256(&current)));
            assert_eq!(would["backup_sha256"], json!(sha256(&shown)));
            let hint = failure.hint.as_deref().unwrap();
            let wanted = match caller {
                Caller::Cli => format!(
                    "--yes --expect-sha256 {} --expect-backup-sha256 {}",
                    sha256(&current),
                    sha256(&shown)
                ),
                Caller::Mcp => format!(
                    "confirm: true, expect_sha256: \"{}\" and expect_backup_sha256: \"{}\"",
                    sha256(&current),
                    sha256(&shown)
                ),
            };
            assert!(hint.contains(&wanted), "{hint}");
        }

        // The backup changes; the config doesn't.
        let swapped = current.replace("10.0.0.1", "10.0.0.2");
        assert_ne!(swapped, current);
        std::fs::write(&backup, &swapped).unwrap();
        let ctx = Context {
            yes: true,
            expect_sha256: Some(sha256(&current)),
            expect_backup_sha256: Some(sha256(&shown)),
            ..cli(path)
        };
        let failure = fails(&ctx, Command::ConfigUndo).await;
        assert_eq!(failure.error, "config_changed", "{failure:?}");
        assert!(
            failure.message.contains("the backup changed"),
            "{failure:?}"
        );
        assert!(failure.hint.as_deref().unwrap().contains("without --yes"));
        assert_eq!(setup.text(), current);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), swapped);

        // The backup that was shown goes back.
        std::fs::write(&backup, &shown).unwrap();
        let undone = ok(&ctx, Command::ConfigUndo).await;
        assert_eq!(undone.json["via"], json!(via), "{}", undone.json);
        assert_eq!(setup.text(), shown);
    }
    assert!(live.state.config().trusted_proxies.is_empty());

    // A tool takes it, and refuses a backup that changed.
    let setup = &offline.setup;
    ok(
        &confirmed(&setup.path, Caller::Cli),
        set("server.trusted-proxies", "[\"10.0.0.1\"]"),
    )
    .await;
    let current = setup.text();
    let backup = setup.dir.path().join("config.yaml.bak");
    let shown = std::fs::read_to_string(&backup).unwrap();
    std::fs::write(&backup, current.replace("10.0.0.1", "10.0.0.2")).unwrap();
    let server = Server::new(Ok(setup.path.clone()), Env::default(), None);
    let mut session = server_session(server).await;
    let result = session
        .call(
            "config_undo",
            json!({
                "confirm": true,
                "expect_sha256": sha256(&current),
                "expect_backup_sha256": sha256(&shown),
            }),
        )
        .await;
    assert_eq!(
        result["structuredContent"]["error"],
        json!("config_changed"),
        "{result}"
    );
    assert!(
        result["structuredContent"]["hint"]
            .as_str()
            .unwrap()
            .contains("without confirm"),
        "{result}"
    );
    let result = session
        .call("config_undo", json!({"expect_backup_sha256": "not-a-hash"}))
        .await;
    assert_eq!(
        result["structuredContent"]["error"],
        json!("usage"),
        "{result}"
    );
    assert!(
        result.to_string().contains("takes the backup_sha256"),
        "{result}"
    );
    assert_eq!(setup.text(), current);
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

// Not upstream's: a server that runs another file holding the same bytes
// as this config counts as running it, as the server doesn't say which
// file it runs. A change then goes to the server's file, and the report
// says this file didn't change; for an undo too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_through_a_copy_says_the_copy_is_unchanged() {
    let live = live(Some(KEY), None).await;
    let copy = Setup::new(live.port, Some(KEY));
    std::fs::write(&copy.path, live.setup.text()).unwrap();
    let ours = copy.text();
    let changed = ok(&cli(&copy.path), set("routing.strategy", "fill-first")).await;
    assert_eq!(changed.json["via"], json!("server"));
    assert_eq!(changed.json["changed"], json!(true));
    let note = changed.json["note"].as_str().unwrap();
    assert!(note.contains("didn't change"), "{note}");
    assert!(note.contains(&copy.path.display().to_string()), "{note}");
    assert_eq!(
        changed.json["changes"],
        json!([{"path": "routing.strategy", "new": "fill-first"}])
    );
    assert!(changed.text.contains("didn't change"));
    assert_eq!(copy.text(), ours);
    assert!(live.setup.text().contains("fill-first"));
    assert_eq!(live.state.config().routing.strategy, "fill-first");

    // An undo from a copy of the server's file and its backup.
    let backup = |setup: &Setup| setup.dir.path().join("config.yaml.bak");
    std::fs::write(&copy.path, live.setup.text()).unwrap();
    std::fs::copy(backup(&live.setup), backup(&copy)).unwrap();
    let ours = copy.text();
    let undone = ok(&confirmed(&copy.path, Caller::Cli), Command::ConfigUndo).await;
    assert_eq!(undone.json["via"], json!("server"));
    assert!(
        undone.json["note"]
            .as_str()
            .unwrap()
            .contains("didn't change"),
        "{}",
        undone.json
    );
    assert!(!undone.json["changes"].as_array().unwrap().is_empty());
    assert_eq!(copy.text(), ours);
    assert!(!live.setup.text().contains("fill-first"));

    // A replacement through a copy: its secrets are in no file read here,
    // but what it changed is scrubbed of them, a key in a URL's path too.
    const PATH_KEY: &str = "sk-placeholder-path-key-0123456789";
    std::fs::write(&copy.path, live.setup.text()).unwrap();
    let replacement = copy.file(
        "replacement.yaml",
        &format!(
            "{}api-keys:\n  codex:\n    - name: \"placeholder\"\n      base-url: \"https://api.example.com/v1/{PATH_KEY}/\"\n      keys:\n        - api-key: \"{PATH_KEY}\"\n",
            config_text(live.port, Some(KEY), &live.setup.auth_dir)
        ),
    );
    let replaced = ok(
        &confirmed(&copy.path, Caller::Cli),
        Command::ConfigReplace(ReplaceInput {
            source: Source::File(replacement),
        }),
    )
    .await;
    assert_eq!(replaced.json["via"], json!("server"));
    assert!(
        replaced.json["changes"]
            .to_string()
            .contains("https://api.example.com/v1/"),
        "{}",
        replaced.json
    );
    assert!(!shows(&replaced, PATH_KEY), "{}", replaced.text);
    assert!(live.setup.text().contains(PATH_KEY));

    // The server's own file is still changed as itself, with no such note.
    let changed = ok(
        &cli(&live.setup.path),
        set("routing.strategy", "fill-first"),
    )
    .await;
    assert!(changed.json.get("note").is_none(), "{}", changed.json);
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

/// Serves a management API that is off, as a server with no key answers
/// it, on `listener`, and writes `text` to the config at `path` each time
/// it is probed, as another write landing then would.
fn edit_on_probe(
    listener: TcpListener,
    path: PathBuf,
    text: Arc<Mutex<String>>,
) -> tokio::task::JoinHandle<()> {
    let app = Router::new().route(
        "/v0/management/debug",
        get_route(move || {
            let written = text.lock().unwrap().clone();
            std::fs::write(&path, written).unwrap();
            async { axum::http::StatusCode::NOT_FOUND }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    })
}

// Not upstream's: a change worked out again, after the file changed under
// it, is made only when it needs no confirmation then: `--yes` was given
// for the change as first worked out, not for the one worked out again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_worked_out_again_gets_no_confirmation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let bare = format!(
        "config-version: 8\nserver:\n  host: \"127.0.0.1\"\n  port: {port}\nmanagement:\n  secret-key: \"{KEY}\"\n"
    );
    let keyed = format!("{bare}access:\n  api-keys:\n    - \"{CLIENT_KEY}\"\n");
    let written = Arc::new(Mutex::new(keyed.clone()));
    let task = edit_on_probe(listener, path.clone(), Arc::clone(&written));
    let ctx = confirmed(&path, Caller::Cli);
    let read = || std::fs::read_to_string(&path).unwrap();

    // Replacing the whole config, which has no client key, as the new one
    // hasn't: a key lands while the server is probed, so the replacement
    // would now remove the last one, which --yes wasn't given for.
    std::fs::write(&path, &bare).unwrap();
    let replacement = dir.path().join("replacement.yaml");
    std::fs::write(
        &replacement,
        format!("{bare}routing:\n  strategy: \"fill-first\"\n"),
    )
    .unwrap();
    let failure = fails(
        &ctx,
        Command::ConfigReplace(ReplaceInput {
            source: Source::File(replacement),
        }),
    )
    .await;
    assert_eq!(failure.error, "config_changed", "{failure:?}");
    assert_eq!(failure.code, exit::FAILED);
    assert!(failure.hint.as_deref().unwrap().contains("run it again"));
    assert_eq!(read(), keyed);

    // A mapping set whole, which needed no confirmation as first worked
    // out: the remote management is turned off meanwhile, and the mapping
    // would turn it back on.
    let remote = |allow: bool| {
        format!(
            "config-version: 8\nserver:\n  host: \"127.0.0.1\"\n  port: {port}\nmanagement:\n  secret-key: \"{KEY}\"\n  allow-remote: {allow}\n"
        )
    };
    std::fs::write(&path, remote(true)).unwrap();
    *written.lock().unwrap() = remote(false);
    let value = dir.path().join("management.json");
    std::fs::write(
        &value,
        json!({
            "secret-key": KEY,
            "allow-remote": true,
            "panel-github-repository": "https://github.com/example/panel"
        })
        .to_string(),
    )
    .unwrap();
    let failure = fails(&ctx, set_from("management", Source::File(value))).await;
    assert_eq!(failure.error, "config_changed", "{failure:?}");
    assert_eq!(read(), remote(false));

    // One that needs no confirmation once worked out again is made, from
    // the file as it is.
    std::fs::write(&path, &bare).unwrap();
    *written.lock().unwrap() = keyed.clone();
    let changed = ok(&cli(&path), set("routing.strategy", "fill-first")).await;
    task.abort();
    assert_eq!(changed.json["via"], json!("file"));
    let text = read();
    assert!(text.contains(CLIENT_KEY), "{text}");
    assert!(text.contains("fill-first"), "{text}");
}

// Not upstream's: `keys add` and `keys remove` work the key list out from
// the file each time the change is worked out, from the bytes the key was
// looked up in, so a key revoked or added between the read and the write
// is kept so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keys_are_worked_out_from_the_file_as_it_is() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let config = |keys: &[&str]| {
        let mut text = format!(
            "config-version: 8\nserver:\n  host: \"127.0.0.1\"\n  port: {port}\nmanagement:\n  secret-key: \"{KEY}\"\naccess:\n  api-keys:\n"
        );
        for key in keys {
            text.push_str(&format!("    - \"{key}\"\n"));
        }
        text
    };
    let revoked = "sk-test-revoked-client-key-0123";
    let added = "sk-test-added-meanwhile-key-4567";
    let new = "sk-test-new-client-key-89abcdef";
    let written = Arc::new(Mutex::new(config(&[CLIENT_KEY])));
    let task = edit_on_probe(listener, path.clone(), Arc::clone(&written));
    let read = || std::fs::read_to_string(&path).unwrap();
    let key_file = dir.path().join("key.txt");
    std::fs::write(&key_file, format!("{new}\n")).unwrap();

    // Adding: a key is revoked while the server is probed; the new key is
    // added to the list without it, and the revoked one stays out.
    std::fs::write(&path, config(&[CLIENT_KEY, revoked])).unwrap();
    let changed = ok(
        &cli(&path),
        Command::KeysAdd(AddInput {
            source: Some(Source::File(key_file.clone())),
            ..AddInput::default()
        }),
    )
    .await;
    assert_eq!(changed.json["via"], json!("file"));
    assert_eq!(changed.json["index"], json!(1));
    let text = read();
    assert!(!text.contains(revoked), "{text}");
    assert!(text.contains(CLIENT_KEY) && text.contains(new), "{text}");

    // Removing by index with --yes: a key is added while the server is
    // probed, and the removal worked out again needs a confirmation it
    // wasn't given, so the file is left as the other write made it.
    std::fs::write(&path, config(&[CLIENT_KEY, revoked])).unwrap();
    *written.lock().unwrap() = config(&[CLIENT_KEY, revoked, added]);
    let ctx = confirmed(&path, Caller::Cli);
    let failure = fails(
        &ctx,
        Command::KeysRemove(RemoveInput {
            index: Some(1),
            ..RemoveInput::default()
        }),
    )
    .await;
    assert_eq!(failure.error, "config_changed", "{failure:?}");
    assert_eq!(read(), config(&[CLIENT_KEY, revoked, added]));

    // Removing a key that is revoked meanwhile: it isn't there to remove.
    std::fs::write(&path, config(&[CLIENT_KEY, revoked])).unwrap();
    *written.lock().unwrap() = config(&[CLIENT_KEY]);
    let failure = fails(
        &ctx,
        Command::KeysRemove(RemoveInput {
            index: Some(1),
            ..RemoveInput::default()
        }),
    )
    .await;
    assert_eq!(failure.error, "not_found", "{failure:?}");
    assert!(!failure_shows(&failure, revoked));
    assert_eq!(read(), config(&[CLIENT_KEY]));

    // Adding a key another write adds meanwhile: it is there already.
    std::fs::write(&path, config(&[CLIENT_KEY])).unwrap();
    *written.lock().unwrap() = config(&[CLIENT_KEY, new]);
    let failure = fails(
        &cli(&path),
        Command::KeysAdd(AddInput {
            source: Some(Source::File(key_file)),
            ..AddInput::default()
        }),
    )
    .await;
    task.abort();
    assert_eq!(failure.error, "exists", "{failure:?}");
    assert_eq!(read(), config(&[CLIENT_KEY, new]));
}

// Not upstream's: a change that needs a confirmation gives the SHA-256 of
// the config it was worked out from; given back with the confirmation, the
// change is made only to that config, and refused once it has changed.
#[tokio::test]
async fn a_confirmation_holds_for_the_config_it_was_shown() {
    let offline = offline(Some(KEY));
    let setup = &offline.setup;
    let sha256 = |text: &str| open_ferry_core::config::save::sha256_hex(text.as_bytes());
    let allow = |more: Value| {
        let mut call = json!({"path": "management.allow-remote", "value": true});
        if let (Some(call), Some(more)) = (call.as_object_mut(), more.as_object()) {
            call.extend(more.clone());
        }
        call
    };

    // Over MCP: the hash comes with what it would change, and in the hint.
    let server = Server::new(Ok(setup.path.clone()), Env::default(), None);
    let mut session = server_session(server).await;
    let shown = setup.text();
    let result = session.call("config_set", allow(json!({}))).await;
    let answer = &result["structuredContent"];
    assert_eq!(answer["error"], json!("needs_confirmation"), "{answer}");
    let given = answer["would"]["config_sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(given, sha256(&shown));
    assert!(
        answer["hint"].as_str().unwrap().contains(&given),
        "{answer}"
    );
    let result = session.call("keys_add", json!({"generate": true})).await;
    assert_eq!(
        result["structuredContent"]["would"]["config_sha256"],
        json!(given)
    );

    // The config changes before the confirmation: refused, and not worked
    // out again from the config as it is.
    let edited = format!("{shown}routing:\n  strategy: fill-first\n");
    std::fs::write(&setup.path, &edited).unwrap();
    let result = session
        .call(
            "config_set",
            allow(json!({"confirm": true, "expect_sha256": given})),
        )
        .await;
    let answer = &result["structuredContent"];
    assert_eq!(answer["error"], json!("config_changed"), "{answer}");
    assert!(answer["hint"].as_str().unwrap().contains("without confirm"));
    assert_eq!(setup.text(), edited);
    let result = session
        .call(
            "config_set",
            allow(json!({"confirm": true, "expect_sha256": "not-a-hash"})),
        )
        .await;
    assert_eq!(result["structuredContent"]["error"], json!("usage"));
    assert_eq!(setup.text(), edited);

    // With the hash of the config as it is, in either case, it is made.
    let result = session
        .call(
            "config_set",
            allow(json!({"confirm": true, "expect_sha256": sha256(&edited).to_uppercase()})),
        )
        .await;
    assert_eq!(
        result["structuredContent"]["changed"],
        json!(true),
        "{result}"
    );
    let got = ok(&cli(&setup.path), get("management.allow-remote")).await;
    assert_eq!(got.json["value"], json!(true));

    // On the command line, for each change that can need a confirmation:
    // a stale hash is refused, whatever --yes says, and nothing changes.
    let before = setup.text();
    let failure = fails(&cli(&setup.path), set("management.allow-remote", "false")).await;
    assert_eq!(failure.error, "needs_confirmation");
    let would = failure.would.clone().unwrap();
    assert_eq!(would["config_sha256"], json!(sha256(&before)));
    assert!(
        failure
            .hint
            .as_deref()
            .unwrap()
            .contains(&format!("--yes --expect-sha256 {}", sha256(&before)))
    );
    let stale = Context {
        yes: true,
        expect_sha256: Some(sha256(&shown)),
        ..cli(&setup.path)
    };
    let whole = setup.file("whole.yaml", &before);
    for command in [
        set("server.host", "0.0.0.0"),
        unset("routing"),
        // One with nothing to unset is refused too.
        unset("requests.proxy-url"),
        Command::ConfigUndo,
        Command::ConfigReplace(ReplaceInput {
            source: Source::File(whole),
        }),
        Command::KeysAdd(AddInput {
            generate: true,
            ..AddInput::default()
        }),
        Command::KeysRemove(RemoveInput {
            index: Some(0),
            ..RemoveInput::default()
        }),
    ] {
        let failure = fails(&stale, command).await;
        assert_eq!(failure.error, "config_changed", "{failure:?}");
        assert_eq!(failure.code, exit::FAILED);
        assert!(failure.hint.as_deref().unwrap().contains("without --yes"));
    }
    assert_eq!(setup.text(), before);

    // A matching one goes through.
    let matching = Context {
        expect_sha256: Some(sha256(&before)),
        ..stale
    };
    let undone = ok(&matching, Command::ConfigUndo).await;
    assert_eq!(undone.json["changed"], json!(true));
    assert_eq!(setup.text(), edited);
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
    let note = changed.json["note"].as_str().unwrap();
    assert!(note.contains("no management key"));
    // A save by the server of what it holds can write over it, and the
    // note says so.
    assert!(note.contains("can undo this change"), "{note}");
    // The server may run another config: the note doesn't say it loads
    // this one's change, only that it does if it runs this config.
    assert!(note.contains("if it runs this config"), "{note}");
    assert!(!note.contains("the server loads"), "{note}");
    let off = super::change::file_note(&super::target::Reach::ManagementOff);
    assert!(off.contains("if it runs this config"), "{off}");
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

/// How many times a test runs a change again that a busy file kept
/// changing under: a writer that keeps losing can still give up, as it
/// should, and a test means to see each change made, not that it never has
/// to be run again.
const RUNS: usize = 5;

// Not upstream's: changes through the agent commands and the management
// API's own writes, made at once, each take the server's write lock, so
// none is lost. One that gives up on a file that kept changing, as it may
// on a busy machine, is run again, as its hint says to.
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
            let mut runs = 0;
            let changed = loop {
                runs += 1;
                match perform(&ctx, set(path, value)).await {
                    Ok(changed) => break changed,
                    Err(failure) if failure.error == "config_changed" && runs < RUNS => {}
                    Err(failure) => panic!("{path} failed after {runs} runs: {failure:?}"),
                }
            };
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
            let mut runs = 0;
            loop {
                runs += 1;
                match live
                    .remote(KEY)
                    .json(
                        Method::PUT,
                        route,
                        Some(Body::Json(json!({"value": value}))),
                    )
                    .await
                {
                    Ok(_) => break,
                    Err(failure) if failure.error == "config_changed" && runs < RUNS => {}
                    Err(failure) => panic!("{route} failed after {runs} runs: {failure:?}"),
                }
            }
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
                    "state-going" => json!({"status": "wait"}),
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

#[derive(serde::Deserialize)]
struct IndexQuery {
    index: usize,
}

// Not upstream's: a key is removed through the server by its index, so the
// list is read again after: when another write moved the keys between the
// read and the delete, and the key removed isn't just the one confirmed,
// it says so, with the keys gone masked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_removed_by_index_is_checked() {
    const OTHER: &str = "sk-another-client-key-qrstuvwxyz0123";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let setup = Setup::new(port, Some(KEY));
    let config = setup.text();
    let keys = Arc::new(Mutex::new(vec![CLIENT_KEY.to_owned(), OTHER.to_owned()]));
    let (listed, deleting) = (Arc::clone(&keys), Arc::clone(&keys));
    let app = Router::new()
        .route(
            "/v0/management/debug",
            get_route(|| async { Json(json!({"debug": false})) }),
        )
        .route(
            "/v0/management/config.yaml",
            get_route(move || async move { config }),
        )
        .route(
            "/v0/management/api-keys",
            get_route(move || {
                let keys = listed.lock().unwrap().clone();
                async move { Json(json!({"api-keys": keys})) }
            })
            .delete(move |Query(query): Query<IndexQuery>| {
                // Another client removes the first key just before this
                // delete lands, so the index now names the next one.
                let mut keys = deleting.lock().unwrap();
                keys.remove(0);
                if query.index < keys.len() {
                    keys.remove(query.index);
                }
                async { Json(json!({"status": "ok"})) }
            }),
        );
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let failure = fails(
        &confirmed(&setup.path, Caller::Cli),
        Command::KeysRemove(RemoveInput {
            index: Some(0),
            source: None,
        }),
    )
    .await;
    task.abort();
    assert_eq!(failure.error, "key_list_changed", "{failure:?}");
    assert_eq!(failure.code, exit::FAILED);
    assert!(failure.message.contains("not just the one confirmed"));
    assert!(failure.message.contains("0123"), "{}", failure.message);
    assert!(failure.hint.as_deref().unwrap().contains("keys list"));
    assert!(!failure_shows(&failure, OTHER));
    assert!(!failure_shows(&failure, CLIENT_KEY));
    assert!(keys.lock().unwrap().is_empty());
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
    // A sign-in still going when the wait ends says to wait again: for a
    // tool, as a result, not an error; on the command line, with exit 1.
    let going = ok(&context(path, Caller::Mcp), login(Some("state-going"))).await;
    assert_eq!(going.json["status"], json!("wait"));
    assert_eq!(going.code, exit::OK);
    assert!(
        going
            .text
            .contains("call credentials_login again with state \"state-going\""),
        "{}",
        going.text
    );
    let result = serde_json::to_value(tool_result(Ok(going))).unwrap();
    assert_ne!(result["isError"], json!(true), "{result}");
    assert_eq!(result["structuredContent"]["status"], json!("wait"));
    let going = ok(&cli(path), login(Some("state-going"))).await;
    assert_eq!(going.json["status"], json!("wait"));
    assert_eq!(going.code, exit::FAILED);
    assert!(going.text.contains("--state state-going"));
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
