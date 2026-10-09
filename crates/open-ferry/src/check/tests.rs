//! `open-ferry check`'s tests: configs in temporary directories, loopback
//! listeners on ephemeral ports, and no real Claude Code.

use std::path::{Path, PathBuf};

use chrono::TimeZone as _;
use tokio::net::{TcpListener, TcpSocket};

use super::*;

mod self_update;
mod separate_address;

/// The time the tests take as now.
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap()
}

fn env() -> Environment {
    Environment {
        now: now(),
        build_date: None,
        dashboard_built: true,
        management_password: false,
        run_config_programs: true,
        updates: super::self_update::Updates {
            mode_env: None,
            trusts_key: true,
            install: open_ferry_update::Install::SelfUpdating {
                binary: PathBuf::from("/usr/local/bin/open-ferry"),
            },
        },
    }
}

/// A port on 127.0.0.1 that nothing listens on, while the socket lives.
fn closed_port() -> (TcpSocket, u16) {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = socket.local_addr().unwrap().port();
    (socket, port)
}

/// A setup in a temporary directory: a config listening on 127.0.0.1 at
/// a port nothing listens on, and an auth directory.
struct Setup {
    dir: tempfile::TempDir,
    port: u16,
    _socket: TcpSocket,
}

impl Setup {
    fn new() -> Self {
        let (socket, port) = closed_port();
        let setup = Self {
            dir: tempfile::tempdir().unwrap(),
            port,
            _socket: socket,
        };
        fs::create_dir(setup.auth_dir()).unwrap();
        setup
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("config.yaml")
    }

    fn auth_dir(&self) -> PathBuf {
        self.dir.path().join("auth")
    }

    /// Writes the config: `server` and `management` lines go into those
    /// sections, `rest` at the end.
    fn write(&self, server: &str, keys: &str, management: &str, rest: &str) {
        let text = format!(
            "config-version: 8\nserver:\n  host: \"127.0.0.1\"\n  port: {}\n{server}\
             management:\n{management}access:\n  api-keys: {keys}\n\
             oauth:\n  auth-dir: '{}'\n{rest}",
            self.port,
            self.auth_dir().display()
        );
        fs::write(self.path(), text).unwrap();
    }

    /// A config that is fine throughout.
    fn write_good(&self) {
        self.write("", "[\"client-key\"]", "  secret-key: \"m\"\n", "");
    }

    fn credential(&self, name: &str, contents: &str) {
        fs::write(self.auth_dir().join(name), contents).unwrap();
    }

    async fn run(&self) -> Vec<Finding> {
        run(&self.path(), &env()).await
    }
}

/// A config listening on `host` and `port`.
fn listening(host: &str, port: i64) -> Config {
    let mut config = Config::default();
    config.host = host.to_owned();
    config.port = port;
    config
}

fn levels(findings: &[Finding]) -> Vec<(Level, &str)> {
    findings
        .iter()
        .map(|finding| (finding.level, finding.check.as_str()))
        .collect()
}

fn find<'a>(findings: &'a [Finding], check: &str) -> &'a Finding {
    findings
        .iter()
        .find(|finding| finding.check == check)
        .unwrap_or_else(|| panic!("no {check} finding in {findings:#?}"))
}

// Not upstream's: a setup with nothing wrong is all ok, and exits with 0.
#[tokio::test]
async fn a_good_setup_is_all_ok() {
    let setup = Setup::new();
    setup.write_good();
    setup.credential(
        "codex-a.json",
        r#"{"type":"codex","email":"a@example.com","last_refresh":"2026-10-07T11:00:00Z","expired":"2026-10-17T11:00:00Z"}"#,
    );
    let findings = setup.run().await;
    assert_eq!(
        levels(&findings),
        [
            (Level::Ok, "config"),
            (Level::Ok, "client keys"),
            (Level::Ok, "management key"),
            (Level::Ok, "auth directory"),
            (Level::Ok, "address"),
            (Level::Ok, "dashboard"),
            (Level::Ok, "clock"),
            (Level::Ok, "self-update"),
        ],
        "{findings:#?}"
    );
    assert_eq!(find(&findings, "client keys").message, "1 key set");
    assert!(
        find(&findings, "auth directory")
            .message
            .ends_with("can be read; 1 credential file loads")
    );
    assert_eq!(
        find(&findings, "address").message,
        format!("nothing listens on 127.0.0.1:{}", setup.port)
    );
    assert_eq!(
        find(&findings, "dashboard").message,
        format!("built in, at http://127.0.0.1:{}/dashboard/", setup.port)
    );
    assert_eq!(
        find(&findings, "clock").message,
        "the system time, 2026-10-07T12:00:00Z, agrees with the credentials' times"
    );
    assert_eq!(exit_code(&findings), ExitCode::SUCCESS);

    fs::remove_file(setup.auth_dir().join("codex-a.json")).unwrap();
    let findings = setup.run().await;
    assert_eq!(
        find(&findings, "clock").message,
        "the system time, 2026-10-07T12:00:00Z, has no build date or credential time to be checked against"
    );
}

// Not upstream's: a config that is missing or doesn't load is the only
// finding, an error.
#[tokio::test]
async fn a_missing_or_broken_config_is_an_error() {
    let setup = Setup::new();
    let findings = setup.run().await;
    assert_eq!(levels(&findings), [(Level::Error, "config")]);
    assert!(findings[0].message.ends_with("config.yaml doesn't exist"));
    assert!(findings[0].fix.contains("open-ferry init -config"));
    assert_eq!(exit_code(&findings), ExitCode::FAILURE);

    fs::write(setup.path(), "server: [\n").unwrap();
    let findings = setup.run().await;
    assert_eq!(levels(&findings), [(Level::Error, "config")]);
    assert!(findings[0].message.contains("doesn't load"), "{findings:?}");

    let findings = run(setup.dir.path(), &env()).await;
    assert_eq!(levels(&findings), [(Level::Error, "config")]);
    assert!(findings[0].message.ends_with("is a directory"));
}

// Not upstream's: safe mode and a busy port are errors; no management key
// is a warning, unless MANAGEMENT_PASSWORD sets one.
#[tokio::test]
async fn safe_mode_and_a_busy_port_are_errors() {
    let setup = Setup::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let busy = listener.local_addr().unwrap().port();
    let text = format!(
        "config-version: 8\nserver:\n  host: \"127.0.0.1\"\n  port: {busy}\n\
         access:\n  api-keys:\n    - \"your-api-key-1\"\n    - \"mine\"\n\
         oauth:\n  auth-dir: '{}'\n",
        setup.auth_dir().display()
    );
    fs::write(setup.path(), text).unwrap();
    let findings = setup.run().await;
    let keys = find(&findings, "client keys");
    assert_eq!(keys.level, Level::Error);
    assert!(keys.message.contains("(your-api-key-1)"));
    assert!(keys.message.contains("safe mode"));
    let management = find(&findings, "management key");
    assert_eq!(management.level, Level::Warning);
    assert!(management.fix.contains("management.secret-key"));
    let address = find(&findings, "address");
    assert_eq!(address.level, Level::Error);
    assert_eq!(
        address.message,
        format!("something already listens on 127.0.0.1:{busy}")
    );
    assert_eq!(exit_code(&findings), ExitCode::FAILURE);

    let with_password = Environment {
        management_password: true,
        ..env()
    };
    let findings = run(&setup.path(), &with_password).await;
    assert_eq!(find(&findings, "management key").level, Level::Ok);
    drop(listener);
}

// Not upstream's: no client key is a warning.
#[tokio::test]
async fn no_client_key_is_a_warning() {
    let setup = Setup::new();
    setup.write("", "[]", "  secret-key: \"m\"\n", "");
    let findings = setup.run().await;
    assert_eq!(find(&findings, "client keys").level, Level::Warning);
    assert_eq!(exit_code(&findings), ExitCode::SUCCESS);
}

// Not upstream's: each bad credential file is named, and none of its
// contents is shown.
#[tokio::test]
async fn names_bad_credential_files_without_showing_them() {
    let setup = Setup::new();
    setup.write_good();
    setup.credential(
        "broken.json",
        "{\"type\":\"codex\",\"access_token\":\"tok-SECRET\",",
    );
    setup.credential("list.JSON", "[\"tok-SECRET\"]");
    setup.credential("empty.json", "");
    setup.credential("untyped.json", "{\"access_token\":\"tok-SECRET\"}");
    setup.credential("gemini.json", "{\"type\":\"gemini\"}");
    setup.credential("weight.json", "{\"type\":\"codex\",\"weight\":1.5}");
    setup.credential("good.json", "{\"type\":\"claude\"}");
    setup.credential("notes.txt", "not a credential");
    fs::create_dir(setup.auth_dir().join("dir.json")).unwrap();
    let findings = setup.run().await;
    let files: Vec<(Level, &str)> = levels(&findings)
        .into_iter()
        .filter(|(_, check)| check.starts_with("credential "))
        .collect();
    assert_eq!(
        files,
        [
            (Level::Error, "credential broken.json"),
            (Level::Warning, "credential empty.json"),
            (Level::Warning, "credential gemini.json"),
            (Level::Error, "credential list.JSON"),
            (Level::Warning, "credential untyped.json"),
            (Level::Error, "credential weight.json"),
        ],
        "{findings:#?}"
    );
    assert_eq!(
        find(&findings, "credential broken.json").message,
        "isn't valid JSON (line 1, column 44)"
    );
    assert_eq!(
        find(&findings, "credential list.JSON").message,
        "isn't a JSON object"
    );
    assert!(
        find(&findings, "credential weight.json")
            .message
            .contains("weight")
    );
    assert!(
        find(&findings, "auth directory")
            .message
            .ends_with("can be read; 1 credential file loads (see below for the others)")
    );
    let text = render_text(&findings);
    assert!(!text.contains("SECRET"));
    assert!(!text.contains("not a credential"));
    assert_eq!(exit_code(&findings), ExitCode::FAILURE);
}

// Not upstream's: an auth directory still to be made is fine; a file in
// its place is an error.
#[tokio::test]
async fn checks_the_auth_directory() {
    let setup = Setup::new();
    setup.write_good();
    fs::remove_dir(setup.auth_dir()).unwrap();
    let findings = setup.run().await;
    let dir = find(&findings, "auth directory");
    assert_eq!(dir.level, Level::Ok);
    assert!(
        dir.message
            .ends_with("doesn't exist yet; the proxy makes it when it starts")
    );

    fs::write(setup.auth_dir(), "").unwrap();
    let findings = setup.run().await;
    let dir = find(&findings, "auth directory");
    assert_eq!(dir.level, Level::Error);
    assert!(dir.message.ends_with("isn't a directory"));
}

// Not upstream's: a system time before the build date, and credential
// times far ahead of it, are warnings.
#[tokio::test]
async fn warns_of_a_clock_that_seems_wrong() {
    let setup = Setup::new();
    setup.write_good();
    setup.credential(
        "ahead.json",
        r#"{"type":"codex","last_refresh":"2026-10-07T13:00:00Z"}"#,
    );
    setup.credential(
        "far.json",
        r#"{"type":"claude","expired":"2028-01-01T00:00:00Z"}"#,
    );
    setup.credential(
        "fine.json",
        r#"{"type":"claude","lastRefresh":"2026-10-07T12:05:00Z","expired":"2027-10-07T00:00:00Z"}"#,
    );
    let env = Environment {
        build_date: Some(Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap()),
        ..env()
    };
    let findings = run(&setup.path(), &env).await;
    let clock: Vec<&str> = findings
        .iter()
        .filter(|finding| finding.check == "clock")
        .map(|finding| {
            assert_eq!(finding.level, Level::Warning);
            finding.message.as_str()
        })
        .collect();
    assert_eq!(
        clock,
        [
            "the system time, 2026-10-07T12:00:00Z, is before this binary's build date, 2026-11-01T00:00:00Z",
            "ahead.json last refreshed at 2026-10-07T13:00:00Z, after the system time",
            "far.json expires at 2028-01-01T00:00:00Z, more than 400 days after the system time",
        ]
    );
    assert_eq!(exit_code(&findings), ExitCode::SUCCESS);

    let findings = setup.run().await;
    let clock = find(&findings, "clock");
    assert_eq!(clock.level, Level::Warning);
}

// Not upstream's: the dashboard missing from the binary is a warning;
// turned off it is fine.
#[tokio::test]
async fn checks_the_dashboard() {
    let setup = Setup::new();
    setup.write_good();
    let without = Environment {
        dashboard_built: false,
        ..env()
    };
    let findings = run(&setup.path(), &without).await;
    assert_eq!(find(&findings, "dashboard").level, Level::Warning);

    setup.write(
        "",
        "[\"k\"]",
        "  secret-key: \"m\"\n  disable-control-panel: true\n",
        "",
    );
    let findings = run(&setup.path(), &without).await;
    let dashboard = find(&findings, "dashboard");
    assert_eq!(dashboard.level, Level::Ok);
    assert!(dashboard.message.contains("disable-control-panel"));
}

// Not upstream's: TLS files that don't load are an error.
#[tokio::test]
async fn checks_tls_when_it_is_on() {
    let setup = Setup::new();
    let missing = setup.dir.path().join("missing.pem").display().to_string();
    setup.write(
        &format!("  tls:\n    enable: true\n    cert: '{missing}'\n    key: '{missing}'\n"),
        "[\"k\"]",
        "  secret-key: \"m\"\n",
        "",
    );
    let findings = setup.run().await;
    let tls = find(&findings, "tls");
    assert_eq!(tls.level, Level::Error);
    assert!(tls.message.contains("missing.pem"));
    assert!(
        find(&findings, "dashboard")
            .message
            .starts_with("built in, at https://")
    );
}

// Not upstream's: a host name or an address that isn't loopback isn't
// connected to; a port of 0 is the system's pick.
#[tokio::test]
async fn connects_only_to_loopback() {
    for host in ["proxy.invalid", "192.0.2.1"] {
        let config = listening(host, 9);
        let mut findings = Vec::new();
        check_address(&config, &mut findings).await;
        assert_eq!(levels(&findings), [(Level::Warning, "address")]);
        assert!(findings[0].message.contains("isn't checked"), "{host}");
    }
    let config = listening("127.0.0.1", 0);
    let mut findings = Vec::new();
    check_address(&config, &mut findings).await;
    assert_eq!(levels(&findings), [(Level::Warning, "address")]);
    let config = listening("127.0.0.1", 70_000);
    let mut findings = Vec::new();
    check_address(&config, &mut findings).await;
    assert_eq!(levels(&findings), [(Level::Error, "address")]);
}

// Not upstream's: every interface is checked on both loopback addresses,
// and a listener on either is found.
#[tokio::test]
async fn checks_every_interface_on_loopback() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let busy = listener.local_addr().unwrap().port();
    for host in ["", "0.0.0.0", "localhost"] {
        let config = listening(host, i64::from(busy));
        let mut findings = Vec::new();
        check_address(&config, &mut findings).await;
        assert_eq!(levels(&findings), [(Level::Error, "address")], "{host:?}");
    }
    let (_socket, free) = closed_port();
    let config = listening("", i64::from(free));
    let mut findings = Vec::new();
    check_address(&config, &mut findings).await;
    assert_eq!(
        findings[0].message,
        format!("nothing listens on 127.0.0.1:{free} or [::1]:{free}")
    );
}

// Not upstream's: only a refusal, a timeout or an address the machine
// doesn't have means the port is free; another failure, such as running
// out of local ports, says nothing of it.
#[test]
fn reads_each_connection_result() {
    use std::io::{Error, ErrorKind};
    assert_eq!(probe(Some(Ok(()))), Probe::Listening);
    assert_eq!(probe(None), Probe::Free);
    for kind in [
        ErrorKind::ConnectionRefused,
        ErrorKind::AddrNotAvailable,
        ErrorKind::NetworkUnreachable,
        ErrorKind::HostUnreachable,
        ErrorKind::NetworkDown,
    ] {
        assert_eq!(probe(Some(Err(Error::from(kind)))), Probe::Free, "{kind:?}");
    }
    // As the system gives it, such as for ::1 on a Linux started with IPv6
    // off.
    assert_eq!(
        probe(Some(Err(Error::from_raw_os_error(EAFNOSUPPORT)))),
        Probe::Free
    );
    for kind in [
        ErrorKind::AddrInUse,
        ErrorKind::PermissionDenied,
        ErrorKind::Other,
    ] {
        assert!(
            matches!(probe(Some(Err(Error::from(kind)))), Probe::Unknown(_)),
            "{kind:?}"
        );
    }
    assert_eq!(
        probe(Some(Err(Error::new(ErrorKind::AddrInUse, "no ports left")))),
        Probe::Unknown("no ports left".to_owned())
    );
}

// Not upstream's: what each claude-cli version check becomes; a missing
// Claude Code is an error. (`tools/fake-claude-cli` tests the checks
// themselves against the fake Claude Code.)
#[tokio::test]
async fn reports_each_claude_cli_entry() {
    let names = ["a".to_owned(), "b".to_owned()];
    let found = |check| {
        let finding = claude_cli_finding(&names, check);
        (finding.level, finding.check, finding.message)
    };
    assert_eq!(
        found(VersionCheck::Supported("2.1.291".into())),
        (
            Level::Ok,
            "claude-cli a, b".into(),
            "Claude Code 2.1.291".into()
        )
    );
    assert_eq!(
        found(VersionCheck::Outdated("2.1.200".into())),
        (
            Level::Error,
            "claude-cli a, b".into(),
            "Claude Code 2.1.200 is older than 2.1.259, which claude-cli needs".into()
        )
    );
    assert_eq!(found(VersionCheck::Unknown("x".into())).0, Level::Warning);
    assert_eq!(found(VersionCheck::Failed("gone".into())).0, Level::Error);

    let setup = Setup::new();
    let missing = setup.dir.path().join("no-such-claude");
    setup.write(
        "",
        "[\"k\"]",
        "  secret-key: \"m\"\n",
        &format!(
            "claude-cli:\n  - name: max\n    command: '{}'\n  - name: off\n    command: '{}'\n    disabled: true\n",
            missing.display(),
            missing.display()
        ),
    );
    let findings = setup.run().await;
    let entries: Vec<&Finding> = findings
        .iter()
        .filter(|finding| finding.check.starts_with("claude-cli"))
        .collect();
    assert_eq!(entries.len(), 1, "{findings:#?}");
    assert_eq!(entries[0].check, "claude-cli max");
    assert_eq!(entries[0].level, Level::Error);
    assert!(entries[0].message.starts_with("couldn't run Claude Code"));
}

// Not upstream's: with `run_config_programs` off, as `open-ferry migrate`
// has it, a claude-cli entry's command isn't run: a command that doesn't
// exist would be an error if it were, and the finding says it wasn't
// checked.
#[tokio::test]
async fn does_not_run_a_claude_cli_command_when_told_not_to() {
    let setup = Setup::new();
    let missing = setup.dir.path().join("no-such-claude");
    setup.write(
        "",
        "[\"k\"]",
        "  secret-key: \"m\"\n",
        &format!(
            "claude-cli:\n  - name: max\n    command: '{}'\n  - name: also\n    command: '{}'\n  - name: off\n    command: '{}'\n    disabled: true\n",
            missing.display(),
            missing.display(),
            missing.display()
        ),
    );
    let env = Environment {
        run_config_programs: false,
        ..env()
    };
    let findings = run(&setup.path(), &env).await;
    let entries: Vec<&Finding> = findings
        .iter()
        .filter(|finding| finding.check.starts_with("claude-cli"))
        .collect();
    assert_eq!(entries.len(), 1, "{findings:#?}");
    assert_eq!(entries[0].check, "claude-cli max, also");
    assert_eq!(entries[0].level, Level::Warning);
    assert!(entries[0].message.starts_with("not checked"));
    assert!(!entries[0].message.contains("couldn't run"));
}

// Not upstream's: a line for each finding and a count, or one JSON
// object.
#[test]
fn renders_lines_or_json() {
    let findings = [
        Finding::ok("config", "c.yaml loads"),
        Finding::warning("management key", "none", "set one"),
        Finding::error("address", "busy", "free it"),
    ];
    assert_eq!(
        render_text(&findings),
        "ok      config: c.yaml loads\n\
         warning management key: none. Fix: set one\n\
         error   address: busy. Fix: free it\n\
         1 error, 1 warning\n"
    );
    assert_eq!(render_text(&[]), "no errors, no warnings\n");
    assert_eq!(
        render_json(Path::new("c.yaml"), &findings),
        json!({
            "config": "c.yaml",
            "status": "error",
            "errors": 1,
            "warnings": 1,
            "findings": [
                {"level": "ok", "check": "config", "message": "c.yaml loads"},
                {"level": "warning", "check": "management key", "message": "none", "fix": "set one"},
                {"level": "error", "check": "address", "message": "busy", "fix": "free it"},
            ],
        })
    );
    assert_eq!(
        render_json(Path::new("c.yaml"), &findings[..1])["status"],
        "ok"
    );
    assert_eq!(exit_code(&findings[..2]), ExitCode::SUCCESS);
    assert_eq!(exit_code(&findings), ExitCode::FAILURE);
}

// Not upstream's: the usage lists the flags, and bad usage exits with 2.
#[test]
fn lists_its_flags_and_turns_down_bad_usage() {
    let usage = usage("open-ferry");
    assert!(usage.starts_with("Usage: open-ferry check [flags]\n"));
    assert!(usage.contains("\n  -config string\n"));
    assert!(usage.contains("\n  -json\n"));
    let run = |args: &[&str]| main("open-ferry", args.iter().map(|arg| (*arg).to_owned()));
    assert_eq!(run(&["-nope"]), ExitCode::from(2));
    assert_eq!(run(&["-config", "x.yaml", "extra"]), ExitCode::from(2));
    assert_eq!(run(&["-json=maybe"]), ExitCode::from(2));
    assert_eq!(run(&["-h"]), ExitCode::SUCCESS);
}
