//! `open-ferry init` and `open-ferry check` as a user runs them: the
//! binary, with configs in temporary directories, and ports on 127.0.0.1
//! that nothing listens on. The keys `init` prints are checked for their
//! shape, never printed.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const OPEN_FERRY: &str = env!("CARGO_BIN_EXE_open-ferry");

/// Runs open-ferry with `args` in `dir`, which has no `.env`.
fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(OPEN_FERRY)
        .args(args)
        .current_dir(dir)
        .env_remove("MANAGEMENT_PASSWORD")
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
