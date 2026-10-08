//! `open-ferry init` and `open-ferry check` as a user runs them: the
//! binary, with configs in temporary directories, and ports on 127.0.0.1
//! that nothing listens on. The keys `init` prints are checked for their
//! shape, never printed. `open-ferry service` is run only with `-h` and
//! bad usage, which it answers before it looks at the system, so no test
//! here installs, removes or asks after a service. The agent commands
//! (`config`, `keys`, `status`, `credentials`, `mcp`) run against a config
//! in a temporary directory whose server port nothing listens on, with no
//! terminal, no management key in the environment, and the directories the
//! installed config would be in pointed at the temporary one.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

const OPEN_FERRY: &str = env!("CARGO_BIN_EXE_open-ferry");

/// Runs open-ferry with `args` in `dir`, which has no `.env`. Its data
/// directory, where check looks for an install receipt, is `dir`'s `data`.
fn run(dir: &Path, args: &[&str]) -> Output {
    let data = dir.join("data");
    Command::new(OPEN_FERRY)
        .args(args)
        .current_dir(dir)
        .env_remove("MANAGEMENT_PASSWORD")
        .env_remove("OPEN_FERRY_SELF_UPDATE")
        .env("LOCALAPPDATA", &data)
        .env("XDG_DATA_HOME", &data)
        .output()
        .unwrap()
}

fn code(output: &Output) -> Option<i32> {
    output.status.code()
}

/// A port on 127.0.0.1 that nothing listens on, while the socket lives.
fn closed_port() -> (tokio::net::TcpSocket, u16) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = socket.local_addr().unwrap().port();
    (socket, port)
}

// Not upstream's: init writes a config that check passes, prints each key
// once, and won't replace it without -force.
#[test]
fn init_writes_a_config_that_check_passes() {
    let dir = tempfile::tempdir().unwrap();
    let (_socket, port) = closed_port();
    let path = dir.path().join("conf").join("config.yaml");
    let shown = path.display().to_string();
    let port_arg = port.to_string();
    let init = run(dir.path(), &["init", "-config", &shown, "-port", &port_arg]);
    assert_eq!(code(&init), Some(0));
    let out = String::from_utf8(init.stdout).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0] == format!("Wrote a new config to {shown}"));
    let key = |label: &str| {
        lines
            .iter()
            .find_map(|line| line.strip_prefix(label))
            .unwrap_or_default()
            .to_owned()
    };
    let client_key = key("Client key:     ");
    let management_key = key("Management key: ");
    assert!(client_key.starts_with("sk-") && client_key.len() == 46);
    assert!(management_key.len() == 64);
    assert!(out.matches(&client_key).count() == 1);
    assert!(out.matches(&management_key).count() == 1);
    assert!(out.contains(&format!(
        "\nDashboard: http://127.0.0.1:{port}/dashboard/\n"
    )));
    assert!(out.contains(" -config "));

    // The template's auth directory is the user's; this one is the test's.
    let auth_dir = dir.path().join("auth");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains(&format!("\n  port: {port}\n")));
    let text = text.replacen(
        "\n  auth-dir: \"~/.cli-proxy-api\"\n",
        &format!("\n  auth-dir: '{}'\n", auth_dir.display()),
        1,
    );
    std::fs::write(&path, text).unwrap();

    let check = run(dir.path(), &["check", "-config", &shown, "-json"]);
    assert_eq!(code(&check), Some(0));
    let out = String::from_utf8(check.stdout).unwrap();
    assert!(!out.contains(&client_key) && !out.contains(&management_key));
    let report: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["errors"], 0, "{report:#}");
    assert_eq!(report["config"], shown.as_str());
    let checks: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|finding| finding["check"].as_str().unwrap())
        .collect();
    assert_eq!(
        checks,
        [
            "config",
            "client keys",
            "management key",
            "auth directory",
            "address",
            "dashboard",
            "clock"
        ]
    );

    let check = run(dir.path(), &["check", "-config", &shown]);
    assert_eq!(code(&check), Some(0));
    let out = String::from_utf8(check.stdout).unwrap();
    assert!(out.starts_with(&format!("ok      config: {shown} loads\n")));
    assert!(out.lines().last().unwrap().starts_with("no errors, "));

    let again = run(dir.path(), &["init", "-config", &shown]);
    assert_eq!(code(&again), Some(1));
    assert!(again.stdout.is_empty());
    let err = String::from_utf8(again.stderr).unwrap();
    assert!(err.contains("already exists; pass -force"));
}

// Not upstream's: check exits with 1 for an error and 2 when it can't
// check; each subcommand has its own -h.
#[test]
fn check_exits_with_its_codes() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.yaml").display().to_string();
    let check = run(dir.path(), &["check", "-config", &missing]);
    assert_eq!(code(&check), Some(1));
    let out = String::from_utf8(check.stdout).unwrap();
    assert!(out.starts_with("error   config: "));
    assert!(out.ends_with("1 error, no warnings\n"));

    let check = run(dir.path(), &["check", "-nope"]);
    assert_eq!(code(&check), Some(2));
    assert!(
        String::from_utf8(check.stderr)
            .unwrap()
            .starts_with("flag provided but not defined: -nope\nUsage: ")
    );

    for subcommand in ["init", "check"] {
        let help = run(dir.path(), &[subcommand, "-h"]);
        assert_eq!(code(&help), Some(0));
        let err = String::from_utf8(help.stderr).unwrap();
        assert!(err.contains(&format!(" {subcommand} [flags]\n")), "{err}");
    }
}

// Not upstream's: with a flag first, a subcommand's name is an argument
// after the flags, as before.
#[test]
fn a_flag_first_reads_the_command_line_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let help = run(dir.path(), &["-h", "init"]);
    assert_eq!(code(&help), Some(0));
    assert!(
        String::from_utf8(help.stderr)
            .unwrap()
            .contains("Usage of ")
    );
    assert!(!dir.path().join("config.yaml").exists());
}

// Not upstream's: service shows its usage with -h, and exits with 2 for bad
// usage before it looks at the system.
#[test]
fn service_shows_its_usage_and_refuses_bad_usage() {
    let dir = tempfile::tempdir().unwrap();
    for args in [&["service", "-h"][..], &["service", "status", "-h"]] {
        let help = run(dir.path(), args);
        assert_eq!(code(&help), Some(0), "{args:?}");
        let err = String::from_utf8(help.stderr).unwrap();
        assert!(
            err.contains(" service install [-config PATH] [-system] [-dry-run]\n"),
            "{err}"
        );
    }
    for (args, message) in [
        (
            &["service"][..],
            "service needs a command: install, uninstall or status",
        ),
        (&["service", "start"], "unknown service command: start"),
        (
            &["service", "status", "-dry-run"],
            "flag provided but not defined: -dry-run",
        ),
        (&["service", "install", "now"], "unexpected argument: now"),
    ] {
        let bad = run(dir.path(), args);
        assert_eq!(code(&bad), Some(2), "{args:?}");
        let err = String::from_utf8(bad.stderr).unwrap();
        assert!(err.starts_with(&format!("{message}\nUsage: ")), "{err}");
        assert!(bad.stdout.is_empty());
    }
}

/// The management key the agent commands' configs hold.
const MANAGEMENT_KEY: &str = "test-management-key-0123456789";

/// The client key the agent commands' configs hold.
const CLIENT_KEY: &str = "sk-test-client-key-abcdefghijklmnop";

/// Writes a config for a server on 127.0.0.1:`port` to `dir`, and gives
/// its path.
fn write_config(dir: &Path, port: u16) -> PathBuf {
    let auth_dir = dir.join("auth");
    std::fs::create_dir_all(&auth_dir).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(
        &path,
        format!(
            "config-version: 8\nserver:\n  host: \"127.0.0.1\"\n  port: {port}\nmanagement:\n  secret-key: \"{MANAGEMENT_KEY}\"\naccess:\n  api-keys:\n    - \"{CLIENT_KEY}\"\noauth:\n  auth-dir: '{}'\n",
            auth_dir.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    path
}

/// open-ferry, to run in `dir` with no management key in its environment
/// and the directories the installed config is in pointed at `dir`.
fn agent_command(dir: &Path) -> Command {
    let mut command = Command::new(OPEN_FERRY);
    command
        .current_dir(dir)
        .env_remove("MANAGEMENT_PASSWORD")
        .env_remove("OPEN_FERRY_MANAGEMENT_KEY_FILE");
    for name in [
        "APPDATA",
        "LOCALAPPDATA",
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
    ] {
        command.env(name, dir);
    }
    command
}

/// Runs open-ferry with `args` in `dir`, with `stdin` as its standard
/// input, which isn't a terminal.
fn run_agent(dir: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = agent_command(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(mut input) = child.stdin.take() {
        // A command that doesn't read it may have exited already.
        let _ = input.write_all(stdin.as_bytes());
    }
    child.wait_with_output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn json_of(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

// Not upstream's: config get, set, unset and undo as a user runs them,
// with no server: text, --json, the exit codes, and a sensitive setting
// that needs --yes with no terminal to ask on.
#[test]
fn config_commands_change_the_file_with_no_server() {
    let dir = tempfile::tempdir().unwrap();
    let (_socket, port) = closed_port();
    let path = write_config(dir.path(), port);
    let config = path.display().to_string();
    let agent = |args: &[&str]| {
        let mut all = args.to_vec();
        all.extend(["--config", config.as_str()]);
        run_agent(dir.path(), &all, "")
    };

    let got = agent(&["config", "get", "routing.strategy"]);
    assert_eq!(code(&got), Some(0));
    assert!(stdout(&got).starts_with("routing.strategy is not set"));

    let set = agent(&["config", "set", "routing.strategy", "fill-first", "--json"]);
    assert_eq!(code(&set), Some(0), "{}", stdout(&set));
    let json = json_of(&set);
    assert_eq!(json["via"], "file");
    assert_eq!(json["changes"][0]["new"], "fill-first");
    assert_eq!(json["undo"], "Undo it with `open-ferry config undo`.");
    let got = agent(&["config", "get", "routing.strategy", "--json"]);
    assert_eq!(json_of(&got)["value"], "fill-first");

    let open = agent(&["config", "set", "server.host", "0.0.0.0"]);
    assert_eq!(code(&open), Some(3));
    let err = stderr(&open);
    assert!(err.contains("needs --yes"), "{err}");
    assert!(
        err.contains("server.host: \"127.0.0.1\" -> \"0.0.0.0\""),
        "{err}"
    );
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("127.0.0.1")
    );
    let open = agent(&["config", "set", "server.host", "0.0.0.0", "--json"]);
    assert_eq!(code(&open), Some(3));
    assert_eq!(json_of(&open)["error"], "needs_confirmation");
    let open = agent(&["config", "set", "server.host", "0.0.0.0", "--yes"]);
    assert_eq!(code(&open), Some(0), "{}", stderr(&open));
    assert!(std::fs::read_to_string(&path).unwrap().contains("0.0.0.0"));

    let undo = agent(&["config", "undo", "--yes"]);
    assert_eq!(code(&undo), Some(0), "{}", stderr(&undo));
    assert!(stdout(&undo).contains("server.host: \"0.0.0.0\" -> \"127.0.0.1\""));
    let diff = agent(&["config", "diff", "--json"]);
    assert_eq!(code(&diff), Some(0));

    let unknown = agent(&["config", "get", "routing.stratgy", "--json"]);
    assert_eq!(code(&unknown), Some(2));
    assert_eq!(json_of(&unknown)["error"], "unknown_path");
    let unknown = agent(&["config", "set", "routing.stratgy", "x"]);
    assert_eq!(code(&unknown), Some(2));
    assert!(stderr(&unknown).contains("routing.strategy"));

    let help = agent(&["config", "--help"]);
    assert_eq!(code(&help), Some(0));
    assert!(stdout(&help).contains("Exit codes: 0 done;"));
    assert_eq!(code(&agent(&["config"])), Some(2));
    assert_eq!(code(&agent(&["config", "nope"])), Some(2));
}

// Not upstream's: with no server, status and the credential commands exit
// with 4 and say how to start it.
#[test]
fn runtime_commands_say_the_server_isnt_running() {
    let dir = tempfile::tempdir().unwrap();
    let (_socket, port) = closed_port();
    let path = write_config(dir.path(), port);
    let config = path.display().to_string();
    let status = run_agent(dir.path(), &["status", "--config", &config, "--json"], "");
    assert_eq!(code(&status), Some(4));
    assert_eq!(json_of(&status)["running"], false);
    let listed = run_agent(
        dir.path(),
        &["credentials", "list", "--config", &config],
        "",
    );
    assert_eq!(code(&listed), Some(4));
    assert!(
        stderr(&listed).contains("open-ferry --config"),
        "{}",
        stderr(&listed)
    );
    let login = run_agent(
        dir.path(),
        &[
            "credentials",
            "login",
            "codex",
            "--config",
            &config,
            "--json",
        ],
        "",
    );
    assert_eq!(code(&login), Some(4));
    assert_eq!(json_of(&login)["error"], "not_running");
}

// Not upstream's: secrets are masked in what the commands print, never
// taken as an argument, and taken from standard input without being
// shown; a new client key is printed once.
#[test]
fn secrets_stay_out_of_arguments_and_output() {
    let dir = tempfile::tempdir().unwrap();
    let (_socket, port) = closed_port();
    let path = write_config(dir.path(), port);
    let config = path.display().to_string();
    for args in [
        &["config", "show"][..],
        &["config", "show", "--json"],
        &["keys", "list"],
        &["keys", "list", "--json"],
        &["config", "get", "management.secret-key"],
        &["status", "--json"],
    ] {
        let mut all = args.to_vec();
        all.extend(["--config", config.as_str()]);
        let output = run_agent(dir.path(), &all, "");
        let out = stdout(&output) + &stderr(&output);
        assert!(!out.contains(CLIENT_KEY), "{args:?}: {out}");
        assert!(!out.contains(MANAGEMENT_KEY), "{args:?}: {out}");
    }
    let reveal = run_agent(
        dir.path(),
        &["keys", "list", "--reveal", "--config", &config],
        "",
    );
    assert_eq!(code(&reveal), Some(3));
    assert!(!stdout(&reveal).contains(CLIENT_KEY));

    let before = std::fs::read_to_string(&path).unwrap();
    let inline = run_agent(
        dir.path(),
        &[
            "config",
            "set",
            "management.secret-key",
            "inline-secret-value",
            "--yes",
            "--config",
            &config,
        ],
        "",
    );
    assert_eq!(code(&inline), Some(2));
    assert!(stderr(&inline).contains("--from-stdin"));
    assert!(!stderr(&inline).contains("inline-secret-value"));
    let flag = run_agent(
        dir.path(),
        &[
            "status",
            "--management-key=flag-secret-value",
            "--config",
            &config,
        ],
        "",
    );
    assert_eq!(code(&flag), Some(2));
    assert!(!stderr(&flag).contains("flag-secret-value"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

    let piped = run_agent(
        dir.path(),
        &[
            "config",
            "set",
            "management.secret-key",
            "--from-stdin",
            "--yes",
            "--config",
            &config,
        ],
        "piped-secret-value-123\n",
    );
    assert_eq!(code(&piped), Some(0), "{}", stderr(&piped));
    let out = stdout(&piped) + &stderr(&piped);
    assert!(!out.contains("piped-secret-value-123"), "{out}");
    assert!(!out.contains(MANAGEMENT_KEY), "{out}");
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("piped-secret-value-123")
    );

    let added = run_agent(
        dir.path(),
        &["keys", "add", "--generate", "--json", "--config", &config],
        "",
    );
    assert_eq!(code(&added), Some(0), "{}", stderr(&added));
    let key = json_of(&added)["key"].as_str().unwrap().to_owned();
    assert!(key.starts_with("sk-") && key.len() == 46);
    assert!(std::fs::read_to_string(&path).unwrap().contains(&key));
}

// Not upstream's: open-ferry mcp speaks MCP on its standard input and
// output, and prints nothing else there; it ends when its input does.
#[test]
fn mcp_serves_on_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let (_socket, port) = closed_port();
    let path = write_config(dir.path(), port);
    let config = path.display().to_string();
    let mut child = agent_command(dir.path())
        .args(["mcp", "--config", &config])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let (lines, received) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let mut answer = |input: &mut std::process::ChildStdin, message: Value| {
        writeln!(input, "{message}").unwrap();
        input.flush().unwrap();
        match received.recv_timeout(Duration::from_secs(60)) {
            Ok(line) => serde_json::from_str::<Value>(&line)
                .unwrap_or_else(|_| panic!("not JSON on stdout: {line}")),
            Err(_) => {
                let _ = child.kill();
                panic!("no answer from open-ferry mcp");
            }
        }
    };
    let init = answer(
        &mut input,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "tests", "version": "0"}},
        }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "open-ferry");
    writeln!(
        input,
        "{}",
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    let tools = answer(
        &mut input,
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    assert_eq!(tools["id"], 2);
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 18);
    let shown = answer(
        &mut input,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "config_show", "arguments": {}},
        }),
    );
    assert_eq!(shown["result"]["isError"], false);
    assert!(!shown.to_string().contains(MANAGEMENT_KEY));
    assert!(!shown.to_string().contains(CLIENT_KEY));
    drop(input);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            panic!("open-ferry mcp didn't end with its input");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success());
    // Nothing else came on stdout.
    assert!(received.recv_timeout(Duration::from_secs(5)).is_err());
}

// Not upstream's: update shows its usage with -h, refuses bad usage, and
// sets self-update.mode in a config; -version prints the version. None of
// these looks for a release, so no test here makes an update request.
#[test]
fn update_shows_its_usage_and_sets_the_mode() {
    let dir = tempfile::tempdir().unwrap();
    let help = run(dir.path(), &["update", "-h"]);
    assert_eq!(code(&help), Some(0));
    let err = String::from_utf8(help.stderr).unwrap();
    assert!(err.contains(" update [flags]\n"), "{err}");
    assert!(
        err.contains("Turn automatic updates off: open-ferry update -mode off\n"),
        "{err}"
    );

    for (args, message) in [
        (
            &["update", "-check", "-rollback"][..],
            "use only one of -check, -rollback and -mode",
        ),
        (
            &["update", "-mode", "sometimes"],
            "-mode is \"sometimes\"; use off, notify or auto",
        ),
        (&["update", "now"], "unexpected argument: now"),
    ] {
        let bad = run(dir.path(), args);
        assert_eq!(code(&bad), Some(2), "{args:?}");
        let err = String::from_utf8(bad.stderr).unwrap();
        assert!(err.starts_with(&format!("{message}\nUsage: ")), "{err}");
        assert!(bad.stdout.is_empty());
    }

    let version = run(dir.path(), &["-version"]);
    assert_eq!(code(&version), Some(0));
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!("open-ferry {}\n", env!("CARGO_PKG_VERSION"))
    );

    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "# Mine.\nport: 8317\n").unwrap();
    let shown = path.display().to_string();
    let set = run(dir.path(), &["update", "-mode", "off", "-config", &shown]);
    assert_eq!(code(&set), Some(0));
    let out = String::from_utf8(set.stdout).unwrap();
    assert!(
        out.contains(&format!("self-update.mode is off in {shown}.\n")),
        "{out}"
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# Mine.\n"), "{text}");
    assert!(text.contains("self-update:\n  mode: off\n"), "{text}");
}
