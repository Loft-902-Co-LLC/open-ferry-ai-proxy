//! The service on Linux: a systemd unit, `open-ferry.service`.
//!
//! By default it is a user unit, in `${XDG_CONFIG_HOME:-~/.config}/systemd/user`,
//! run by the user's own systemd instance (`systemctl --user`): it starts
//! when they log in, and, with `loginctl enable-linger`, at boot, and keeps
//! running after they log out. With `-system` it is a system unit in
//! `/etc/systemd/system`, run as root at boot once the network is up.
//! Either restarts it 5 seconds after it fails, and journald keeps what it
//! prints.

use std::fmt::Write as _;

use super::{Cmd, Context, DOCS, Definition, Gone, Platform, Step, StopCheck};

/// The unit's name.
pub(super) const UNIT: &str = "open-ferry.service";

/// Where the unit goes.
pub(super) fn unit_path(context: &Context, system: bool) -> Result<String, String> {
    if system {
        return Ok(format!("/etc/systemd/system/{UNIT}"));
    }
    let dir = Platform::Linux.join(&context.config_home()?, "systemd/user");
    Ok(Platform::Linux.join(&dir, UNIT))
}

/// The unit that runs `definition`.
///
/// A supervised unit runs `-supervise -config <path> -data-dir <path>`, with
/// `KillMode=mixed` and `TimeoutStopSec=60`. Stopping the unit sends SIGTERM
/// to the supervisor alone, which asks its workers to end and waits for them
/// for at most `STOP_BUDGET` (50 s), inside the 60; if the unit is still
/// running at 60 s, systemd sends SIGKILL to every process in the unit's
/// cgroup. `mixed` therefore promises no more than that: an orderly end for
/// the supervisor's own SIGTERM, and a forced end for whatever is left. It
/// does not make the workers' own shutdown any faster or surer. Without it
/// (`control-group`, the default) the SIGTERM would reach the workers too,
/// before the supervisor has handed their sockets over or drained them.
pub(super) fn unit(definition: &Definition, system: bool) -> String {
    let flag = if system { " -system" } else { "" };
    let mut unit = format!(
        "# Installed by `open-ferry service install{flag}`; `open-ferry service uninstall{flag}` removes it.
[Unit]
Description=open-ferry AI proxy
Documentation={DOCS}
"
    );
    if system {
        // A user unit can't wait for the network: the user's systemd
        // doesn't know when the system's is up.
        unit.push_str("Wants=network-online.target\nAfter=network-online.target\n");
    }
    let command = match &definition.data_dir {
        Some(data_dir) => format!(
            "{} -supervise{flag} -config {} -data-dir {}",
            quote(&definition.exe),
            quote(&definition.config),
            quote(data_dir)
        ),
        None => format!(
            "{} -config {}",
            quote(&definition.exe),
            quote(&definition.config)
        ),
    };
    let stop = if definition.data_dir.is_some() {
        "KillMode=mixed\nTimeoutStopSec=60\n"
    } else {
        ""
    };
    let _ = write!(
        unit,
        "
[Service]
Type=simple
ExecStart={command}
WorkingDirectory={}
Restart=on-failure
RestartSec=5
{stop}
[Install]
WantedBy={}
",
        definition.dir.replace('%', "%%"),
        if system {
            "multi-user.target"
        } else {
            "default.target"
        },
    );
    unit
}

/// The words of a unit's `ExecStart` line, unquoted as [`quote`] quotes them.
/// Only what the installer writes, and plain hand edits of it, is read:
/// exactly one `ExecStart=` in `[Service]`, no continuation lines, no
/// prefix, single quote, unknown escape, unfinished quote or lone `%` or `$`
/// (which systemd would expand). Anything else is `None`.
pub(super) fn exec_words(unit: &str) -> Option<Vec<String>> {
    let mut in_service = false;
    let mut line_of = None;
    // systemd splits and trims on ASCII whitespace only.
    let blank = |c: char| c.is_ascii_whitespace();
    for line in unit.lines() {
        let line = line.trim_matches(blank);
        if in_service && line.ends_with('\\') {
            return None;
        }
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') {
            in_service = line == "[Service]";
            continue;
        }
        if !in_service {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim_end_matches(blank) == "ExecStart"
            && line_of.replace(value.trim_start_matches(blank)).is_some()
        {
            return None;
        }
    }
    let line = line_of?;
    if line.starts_with(['-', '@', ':', '+', '!']) {
        return None;
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\'' if !quoted => return None,
            '\\' if quoted => {
                // `quote` escapes `\` and `"` only.
                match chars.next() {
                    Some(next @ ('\\' | '"')) => word.push(next),
                    _ => return None,
                }
            }
            '\\' => return None,
            // `quote` doubles `%` and `$` so that systemd doesn't expand them.
            '%' | '$' => {
                if chars.next() != Some(c) {
                    return None;
                }
                word.push(c);
                started = true;
            }
            c if blank(c) && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            // Other whitespace is part of a word to systemd, and to us only
            // in quotes.
            c if c.is_whitespace() && !quoted => return None,
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if started {
        words.push(word);
    }
    // systemd takes its prefixes from the decoded executable too.
    if words.first()?.starts_with(['-', '@', ':', '+', '!']) {
        return None;
    }
    Some(words)
}

/// A word of `ExecStart`, in double quotes: `\` and `"` escaped, and `%`
/// and `$` doubled so that systemd doesn't expand them.
fn quote(word: &str) -> String {
    let mut quoted = String::from('"');
    for c in word.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '$' => quoted.push_str("$$"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

fn systemctl(system: bool, args: &[&str]) -> Cmd {
    let mut cmd = Cmd::new("systemctl", &[]);
    if !system {
        cmd = cmd.arg("--user");
    }
    args.iter().fold(cmd, |cmd, arg| cmd.arg(*arg))
}

/// Writes the unit at `path`, then enables and starts it.
pub(super) fn install_steps(path: &str, definition: &Definition, system: bool) -> Vec<Step> {
    vec![
        Step::Write {
            path: path.to_owned(),
            text: unit(definition, system),
            utf16: false,
        },
        Step::Run(systemctl(system, &["daemon-reload"])),
        Step::Run(systemctl(system, &["enable", "--now", UNIT])),
    ]
}

/// Stops and disables the unit at `path`, then removes it.
pub(super) fn uninstall_steps(path: &str, system: bool) -> Vec<Step> {
    vec![
        Step::TryRun(systemctl(system, &["disable", "--now", UNIT])),
        Step::Remove(path.to_owned()),
        Step::Run(systemctl(system, &["daemon-reload"])),
    ]
}

/// What replaces [`uninstall_steps`]' first step for a service that may run
/// the supervisor: where the stop fails, `is-active` must say the unit isn't
/// running before the rest goes on.
pub(super) fn stop_check(system: bool) -> StopCheck {
    StopCheck {
        stop: systemctl(system, &["disable", "--now", UNIT]),
        query: systemctl(system, &["is-active", UNIT]),
        gone: Gone::Systemd,
    }
}

pub(super) fn status(system: bool) -> Cmd {
    systemctl(system, &["--no-pager", "status", UNIT])
}

pub(super) fn notes(system: bool) -> String {
    if system {
        "open-ferry is installed and started, as a systemd system service: it starts at boot, and again when it fails.
Its logs: journalctl -u open-ferry".to_owned()
    } else {
        "open-ferry is installed and started, as a systemd user service: it starts when you log in, and again when it fails.
Its logs: journalctl --user -u open-ferry
To start it at boot and keep it running when you are logged out, run: loginctl enable-linger".to_owned()
    }
}
