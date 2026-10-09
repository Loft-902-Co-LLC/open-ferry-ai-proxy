//! The record of a switch, which `open-ferry migrate -undo` reads:
//! `migration.json` beside the installed config (`~/.config/open-ferry/`
//! on Linux and macOS, `%APPDATA%\open-ferry\` on Windows; root's when run
//! with `sudo`), with a copy in the backup.
//!
//! It holds paths, names and states only: no command line, no
//! environment, no file contents.

use serde::{Deserialize, Serialize};

use super::machine::Machine;
use crate::os_service::{Context, Platform, Target};

/// The record's file name.
pub(crate) const FILE: &str = "migration.json";

/// The record's format version.
pub(crate) const VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Status {
    /// The switch began and hasn't finished: it failed in a way that
    /// couldn't be undone, or `migrate` was stopped.
    Switching,
    Switched,
    /// The switch failed, and was undone.
    RolledBack,
    /// `-undo` switched back.
    Undone,
}

/// Where CLIProxyAPI listened.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Address {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) tls: bool,
}

/// CLIProxyAPI as it was found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Before {
    pub(crate) exe: Option<String>,
    pub(crate) config: String,
    pub(crate) working_dir: Option<String>,
    pub(crate) auth_dir: Option<String>,
    pub(crate) listen: Option<Address>,
    /// What started it, in words.
    pub(crate) started_by: String,
}

/// A file or directory copied into the backup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Copied {
    pub(crate) from: String,
    pub(crate) to: String,
    /// Whether it is a directory, copied with what it holds.
    #[serde(default)]
    pub(crate) dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Backup {
    pub(crate) dir: String,
    pub(crate) files: Vec<Copied>,
}

/// CLIProxyAPI's service, as it was before the switch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "manager", rename_all = "kebab-case")]
pub(crate) enum Theirs {
    Systemd {
        unit: String,
        user: bool,
        was_enabled: bool,
        was_active: bool,
    },
    Launchd {
        label: String,
        plist: String,
        domain: String,
        was_loaded: bool,
    },
    WindowsService {
        name: String,
        /// `sc config`'s `start=` value it had.
        start_type: String,
        was_running: bool,
    },
    Task {
        name: String,
        was_enabled: bool,
        was_running: bool,
    },
}

/// How the switch was made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum Switch {
    /// CLIProxyAPI's service stopped and disabled, open-ferry's installed
    /// as `ours` (see [`target_name`]).
    Service { theirs: Theirs, ours: String },
    /// open-ferry's binary put in place of CLIProxyAPI's, `binary`, which
    /// was moved to `moved_to`.
    DropIn {
        binary: String,
        moved_to: String,
        /// Whether `migrate` stopped CLIProxyAPI and started open-ferry.
        restarted: bool,
        /// CLIProxyAPI's process at the switch: `-undo` leaves it running
        /// when it wasn't restarted.
        pid: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) version: u32,
    /// When the switch began and when the record last changed, in UTC
    /// (RFC 3339).
    pub(crate) created: String,
    pub(crate) updated: String,
    pub(crate) status: Status,
    /// `linux`, `macos` or `windows`.
    pub(crate) platform: String,
    pub(crate) cliproxyapi: Before,
    pub(crate) backup: Option<Backup>,
    pub(crate) switch: Switch,
}

pub(crate) fn platform_name(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => "linux",
        Platform::MacOs => "macos",
        Platform::Windows => "windows",
    }
}

/// A service target's name in the record.
pub(crate) fn target_name(target: Target) -> &'static str {
    match target {
        Target::SystemdUser => "systemd-user",
        Target::SystemdSystem => "systemd-system",
        Target::LaunchAgent => "launch-agent",
        Target::LaunchDaemon => "launch-daemon",
        Target::ScheduledTask => "scheduled-task",
        Target::WindowsService => "windows-service",
    }
}

/// The service target a record names.
pub(crate) fn target_of(name: &str) -> Option<Target> {
    [
        Target::SystemdUser,
        Target::SystemdSystem,
        Target::LaunchAgent,
        Target::LaunchDaemon,
        Target::ScheduledTask,
        Target::WindowsService,
    ]
    .into_iter()
    .find(|target| target_name(*target) == name)
}

/// Where the record is kept: beside the installed config.
pub(crate) fn path(context: &Context) -> Result<String, String> {
    let config = context
        .installed_config
        .as_ref()
        .map_err(|error| format!("there is nowhere to keep the switch's record: {error}"))?;
    let dir = context.platform.parent(config).ok_or_else(|| {
        format!("there is nowhere to keep the switch's record: {config} has no directory")
    })?;
    Ok(context.platform.join(&dir, FILE))
}

/// Reads the record at `path`: `Ok(None)` when there is none.
pub(crate) fn load(machine: &dyn Machine, path: &str) -> Result<Option<Record>, String> {
    if !machine.exists(path) {
        return Ok(None);
    }
    let data = machine
        .read(path)
        .map_err(|error| format!("failed to read the switch's record, {path}: {error}"))?;
    let record: Record = serde_json::from_slice(&data)
        .map_err(|error| format!("the switch's record, {path}, doesn't read: {error}"))?;
    if record.version != VERSION {
        return Err(format!(
            "the switch's record, {path}, is of version {}, which this open-ferry doesn't read",
            record.version
        ));
    }
    Ok(Some(record))
}

/// Writes `record` to `path`, and to its copy in the backup.
pub(crate) fn save(
    machine: &mut dyn Machine,
    platform: Platform,
    path: &str,
    record: &Record,
) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(record)
        .map_err(|error| format!("failed to write the switch's record: {error}"))?;
    text.push('\n');
    if let Some(dir) = platform.parent(path)
        && !machine.exists(&dir)
    {
        machine
            .create_dir_all(&dir)
            .map_err(|error| format!("failed to create {dir}: {error}"))?;
    }
    machine
        .write(path, text.as_bytes())
        .map_err(|error| format!("failed to write the switch's record, {path}: {error}"))?;
    if let Some(backup) = &record.backup {
        let copy = platform.join(&backup.dir, FILE);
        machine
            .write(&copy, text.as_bytes())
            .map_err(|error| format!("failed to write {copy}: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not upstream's: the record reads back as it was written, in the
    // documented field names.
    #[test]
    fn record_round_trips() {
        let record = Record {
            version: VERSION,
            created: "2026-10-08T12:00:00Z".to_owned(),
            updated: "2026-10-08T12:00:30Z".to_owned(),
            status: Status::Switched,
            platform: "linux".to_owned(),
            cliproxyapi: Before {
                exe: Some("/opt/cpa/cli-proxy-api".to_owned()),
                config: "/opt/cpa/config.yaml".to_owned(),
                working_dir: Some("/opt/cpa".to_owned()),
                auth_dir: Some("/home/me/.cli-proxy-api".to_owned()),
                listen: Some(Address {
                    host: String::new(),
                    port: 8317,
                    tls: false,
                }),
                started_by: "the systemd user service cliproxyapi.service".to_owned(),
            },
            backup: None,
            switch: Switch::Service {
                theirs: Theirs::Systemd {
                    unit: "cliproxyapi.service".to_owned(),
                    user: true,
                    was_enabled: true,
                    was_active: true,
                },
                ours: target_name(Target::SystemdUser).to_owned(),
            },
        };
        let text = serde_json::to_string(&record).unwrap_or_default();
        assert!(text.contains(r#""status":"switched""#), "{text}");
        assert!(text.contains(r#""kind":"service""#), "{text}");
        assert!(text.contains(r#""manager":"systemd""#), "{text}");
        assert!(text.contains(r#""was_enabled":true"#), "{text}");
        let read: Record = serde_json::from_str(&text).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(read, record);
        assert_eq!(target_of("windows-service"), Some(Target::WindowsService));
        assert_eq!(target_of("nothing"), None);
    }
}
