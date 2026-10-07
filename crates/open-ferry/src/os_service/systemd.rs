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

use super::{Cmd, Context, DOCS, Definition, Platform, Step};

/// The unit's name.
const UNIT: &str = "open-ferry.service";

/// Where the unit goes.
pub(super) fn unit_path(context: &Context, system: bool) -> Result<String, String> {
    if system {
        return Ok(format!("/etc/systemd/system/{UNIT}"));
    }
    let dir = Platform::Linux.join(&context.config_home()?, "systemd/user");
    Ok(Platform::Linux.join(&dir, UNIT))
}

/// The unit that runs `definition`.
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
    let _ = write!(
        unit,
        "
[Service]
Type=simple
ExecStart={} -config {}
WorkingDirectory={}
Restart=on-failure
RestartSec=5

[Install]
WantedBy={}
",
        quote(&definition.exe),
        quote(&definition.config),
        definition.dir.replace('%', "%%"),
        if system {
            "multi-user.target"
        } else {
            "default.target"
        },
    );
    unit
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
