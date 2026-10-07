//! `fake-claude`: a stand-in for Claude Code, for open-ferry's `claude-cli`
//! tests, which must never run the real one. It reaches nothing: it reads a
//! scenario, records how it was run, and prints canned output.
//!
//! Its directory is `$CLAUDE_CONFIG_DIR`, which `claude-cli` sets from an
//! entry's `config-dir`, else `$FAKE_CLAUDE_DIR`. In it:
//!
//! - `scenario.json` says what to do (every field may be left out):
//!   - `stdout`: lines to print, each a JSON value written on one line, or
//!     a string printed as it is;
//!   - `stderr`: text for standard error;
//!   - `delay_ms`: a pause before each line; `start_delay_ms`: one before
//!     the first;
//!   - `hang`: after the lines, run until killed, adding to
//!     `heartbeat-<pid>` every 20 ms, so a test can tell it was stopped;
//!   - `exit_code`: the exit code, 0 by default;
//!   - `record_values`: the variables whose values are recorded, besides
//!     those `claude-cli` sets (`CLAUDE_CONFIG_DIR`,
//!     `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `MAX_THINKING_TOKENS`); only ones
//!     the test set itself belong here;
//!   - `version`: what `--version` prints;
//!   - `auth_status` and `auth_exit_code`: what `auth status` prints, and
//!     its exit code.
//! - `record-<pid>.json` is written for each run in print mode, when it
//!   starts and again when it ends: its arguments, working directory,
//!   variable names, the values asked for, standard input, the system
//!   prompt file's contents, and its start and end times in milliseconds.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

/// The variables whose values are always recorded: those `claude-cli` sets.
const SET_BY_CLAUDE_CLI: [&str; 3] = [
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
    "MAX_THINKING_TOKENS",
];

/// What `auth status` prints by default: more than `claude-cli` passes on.
const AUTH_STATUS: &str = r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"someone@example.com","orgId":"org-123","orgName":"Example Org","configDirectory":"/home/someone/.claude","subscriptionType":"max"}"#;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = directory();
    let scenario = dir
        .as_deref()
        .and_then(|dir| std::fs::read(dir.join("scenario.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    let text = |name: &str| scenario.get(name).and_then(Value::as_str);
    let number = |name: &str| scenario.get(name).and_then(Value::as_u64);
    let code = |name: &str| {
        scenario
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::SUCCESS, ExitCode::from)
    };

    if args.first().map(String::as_str) == Some("--version") {
        println!("{}", text("version").unwrap_or("2.1.291 (Claude Code)"));
        return ExitCode::SUCCESS;
    }
    if args.first().map(String::as_str) == Some("auth")
        && args.get(1).map(String::as_str) == Some("status")
    {
        println!("{}", text("auth_status").unwrap_or(AUTH_STATUS));
        return code("auth_exit_code");
    }

    let started = now_ms();
    let mut stdin = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin);
    let mut record = json!({
        "argv": args,
        "cwd": std::env::current_dir().map(|dir| dir.display().to_string()).unwrap_or_default(),
        "env_names": env_names(),
        "env": env_values(&scenario),
        "stdin": stdin,
        "prompt": prompt(&args),
        "start_ms": started,
        "end_ms": null,
    });
    if let Some(dir) = dir.as_deref() {
        write_record(dir, &record);
    }

    if let Some(text) = text("stderr") {
        eprint!("{text}");
    }
    if let Some(delay) = number("start_delay_ms") {
        std::thread::sleep(Duration::from_millis(delay));
    }
    let delay = number("delay_ms").map(Duration::from_millis);
    let mut stdout = std::io::stdout().lock();
    for line in scenario
        .get("stdout")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(delay) = delay {
            std::thread::sleep(delay);
        }
        let text = match line {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        if writeln!(stdout, "{text}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            // The reader is gone.
            return ExitCode::from(141);
        }
    }
    drop(stdout);
    if scenario.get("hang").and_then(Value::as_bool) == Some(true) {
        hang(dir.as_deref());
    }
    record["end_ms"] = Value::from(now_ms());
    if let Some(dir) = dir.as_deref() {
        write_record(dir, &record);
    }
    code("exit_code")
}

/// The fake's directory: `$CLAUDE_CONFIG_DIR`, else `$FAKE_CLAUDE_DIR`.
fn directory() -> Option<PathBuf> {
    ["CLAUDE_CONFIG_DIR", "FAKE_CLAUDE_DIR"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .find(|dir| dir.join("scenario.json").is_file())
}

/// The names of the variables it was given, sorted.
fn env_names() -> Vec<String> {
    let mut names: Vec<String> = std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The values of the variables `claude-cli` sets and those the scenario
/// names, for those it was given.
fn env_values(scenario: &Value) -> Map<String, Value> {
    let named = scenario
        .get("record_values")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    let mut values = BTreeMap::new();
    for name in SET_BY_CLAUDE_CLI.into_iter().chain(named) {
        if let Some(value) = std::env::var_os(name) {
            values.insert(name.to_owned(), value.to_string_lossy().into_owned());
        }
    }
    values
        .into_iter()
        .map(|(name, value)| (name, Value::String(value)))
        .collect()
}

/// The system prompt file's flag, path and contents.
fn prompt(args: &[String]) -> Value {
    let Some(at) = args
        .iter()
        .position(|arg| arg == "--system-prompt-file" || arg == "--append-system-prompt-file")
    else {
        return Value::Null;
    };
    let path = args.get(at + 1).cloned().unwrap_or_default();
    json!({
        "flag": args[at],
        "path": path,
        "contents": std::fs::read_to_string(&path).ok(),
    })
}

fn write_record(dir: &Path, record: &Value) {
    let path = dir.join(format!("record-{}.json", std::process::id()));
    let temp = dir.join(format!("record-{}.tmp", std::process::id()));
    if std::fs::write(&temp, record.to_string()).is_ok() {
        let _ = std::fs::rename(&temp, &path);
    }
}

/// Runs until killed, adding to `heartbeat-<pid>` in `dir`.
fn hang(dir: Option<&Path>) -> ! {
    let heartbeat = dir.map(|dir| dir.join(format!("heartbeat-{}", std::process::id())));
    loop {
        if let Some(path) = &heartbeat
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        {
            let _ = file.write_all(b".");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}
