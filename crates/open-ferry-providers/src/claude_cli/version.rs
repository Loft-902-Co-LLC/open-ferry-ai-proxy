//! Checking that an entry's Claude Code is new enough for the flags
//! `claude-cli` passes.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use open_ferry_core::config::ClaudeCli;
use tokio::process::Command;

use super::events::mask;
use super::process::no_window;
use super::settings::Entry;

/// The oldest Claude Code `claude-cli` runs: the first with
/// `--permission-prompts`, which also takes `client_composed` input.
pub const MIN_VERSION: (u32, u32, u32) = (2, 1, 259);

/// How long `--version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);

/// What an entry's Claude Code said of its version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionCheck {
    /// Its version, new enough.
    Supported(String),
    /// Its version, older than [`MIN_VERSION`].
    Outdated(String),
    /// It ran, but gave no version; what it printed, shortened and masked.
    Unknown(String),
    /// It couldn't be run, and why.
    Failed(String),
}

/// Runs the entry's Claude Code with `--version`, in the entry's
/// environment, and reads the first `x.y.z` it prints.
pub async fn check_version(entry: &Entry) -> VersionCheck {
    let program = match entry.program() {
        Ok(program) => program,
        Err(error) => return VersionCheck::Failed(error),
    };
    let mut command = Command::new(&program);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    entry.apply_env(&mut command);
    no_window(&mut command);
    let output = match tokio::time::timeout(VERSION_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return VersionCheck::Failed(format!("couldn't run {}: {error}", program.display()));
        }
        Err(_) => {
            return VersionCheck::Failed(format!(
                "{} --version didn't finish within {}s",
                program.display(),
                VERSION_TIMEOUT.as_secs()
            ));
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    match parse_version(&text) {
        Some(version) if version >= MIN_VERSION => VersionCheck::Supported(format_version(version)),
        Some(version) => VersionCheck::Outdated(format_version(version)),
        None => VersionCheck::Unknown(mask(text.trim(), 200)),
    }
}

/// Checks the Claude Code of each enabled entry in `entries` once, in the
/// background, and logs its version, or a warning when it is missing, too
/// old or unreadable. Entries that run the same command are checked once.
/// Does nothing outside a Tokio runtime.
pub fn warn_outdated(entries: &[ClaudeCli]) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut commands: BTreeMap<String, (Entry, Vec<String>)> = BTreeMap::new();
    for entry in entries.iter().filter(|entry| !entry.disabled) {
        let entry = Entry::from_config(entry);
        let names = &mut commands
            .entry(entry.command.clone())
            .or_insert_with(|| (entry.clone(), Vec::new()))
            .1;
        names.push(entry.name);
    }
    for (entry, names) in commands.into_values() {
        handle.spawn(async move {
            let check = check_version(&entry).await;
            log_check(&names.join(", "), &check);
        });
    }
}

/// Logs what [`check_version`] found for the entries `names`.
fn log_check(names: &str, check: &VersionCheck) {
    let (min_major, min_minor, min_patch) = MIN_VERSION;
    match check {
        VersionCheck::Supported(version) => {
            tracing::info!("claude-cli {names}: Claude Code {version}");
        }
        VersionCheck::Outdated(version) => tracing::warn!(
            "claude-cli {names}: Claude Code {version} is older than \
             {min_major}.{min_minor}.{min_patch}, which claude-cli needs; update it with `claude update`"
        ),
        VersionCheck::Unknown(output) => tracing::warn!(
            "claude-cli {names}: Claude Code gave no version (it printed {output:?}); \
             claude-cli needs {min_major}.{min_minor}.{min_patch} or later"
        ),
        VersionCheck::Failed(error) => {
            tracing::warn!("claude-cli {names}: couldn't check Claude Code's version: {error}")
        }
    }
}

/// The first `x.y.z` in `text`.
fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|word| {
            let mut parts = word.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            let patch = parts.next()?.parse().ok()?;
            Some((major, minor, patch))
        })
}

fn format_version((major, minor, patch): (u32, u32, u32)) -> String {
    format!("{major}.{minor}.{patch}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_first_version() {
        assert_eq!(parse_version("2.1.291 (Claude Code)\n"), Some((2, 1, 291)));
        assert_eq!(parse_version("claude v10.0.3-beta"), Some((10, 0, 3)));
        assert_eq!(parse_version("1.2 then 3.4.5"), Some((3, 4, 5)));
        assert_eq!(parse_version("no version"), None);
        assert!((2, 1, 291) >= MIN_VERSION);
        assert!((2, 1, 258) < MIN_VERSION);
        assert!((3, 0, 0) >= MIN_VERSION);
    }

    #[tokio::test]
    async fn a_missing_program_fails() {
        let entry = Entry {
            command: "open-ferry-no-such-claude".into(),
            ..Entry::from_auth(&open_ferry_core::auth::Auth::default())
        };
        assert!(matches!(
            check_version(&entry).await,
            VersionCheck::Failed(error) if error.contains("isn't on PATH")
        ));
    }
}
