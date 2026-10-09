//! The service on Windows: a scheduled task, or with `-system` a Windows
//! service.
//!
//! A config's `~` paths, the auth directory's among them, are the profile
//! of the account open-ferry runs as. A Windows service runs as a service
//! account, LocalSystem here, whose profile isn't the user's, so it
//! wouldn't see their sign-ins; one that ran as the user would need their
//! password stored with the service manager. So by default the service is
//! a scheduled task, `open-ferry`, that runs as the user who installed it,
//! when they log on, with no password stored and no administrator needed to
//! register it. It runs `open-ferry service run -config <path>`, which
//! starts the server with no console window and starts it again 5 seconds
//! after it fails, waiting longer each time it fails again soon (see
//! `run`). What the server prints goes to `service.log` in the config's
//! directory, kept under 10 MiB by moving it to `service.log.1` as the
//! server starts.
//!
//! With `-system` it is a Windows service, `open-ferry`, that runs as
//! LocalSystem from boot: `open-ferry service run -system -config <path>`,
//! which answers the service manager and serves in the same process. The
//! service manager starts it again 5 seconds after it fails. It prints to
//! the same `service.log`, and the service manager logs its starts and
//! stops in the System event log. Its config's auth directory must be a
//! full path, and the binary and the config must be where only
//! administrators can change them.

use std::fmt::Write as _;

use super::launchd::escape;
use super::{Cmd, Definition, Platform, SERVICE_NAME, Step, System, run_checked};

/// Where the server's output goes, in the config's directory.
pub(super) const LOG_FILE: &str = "service.log";

/// `sc.exe`'s exit code for a service that doesn't exist
/// (`ERROR_SERVICE_DOES_NOT_EXIST`).
pub(super) const SERVICE_DOES_NOT_EXIST: i32 = 1060;

/// The SIDs of the integrity levels an administrator's elevated process
/// has: high, and system.
const ELEVATED_LEVELS: [&str; 2] = ["S-1-16-12288", "S-1-16-16384"];

/// Whether this process runs elevated, as an administrator.
pub(super) fn elevated(system: &mut dyn System) -> Result<bool, String> {
    let output = run_checked(
        system,
        &Cmd::new("whoami.exe", &["/groups", "/fo", "csv", "/nh"]),
    )?;
    Ok(output.stdout.lines().any(|line| {
        line.split(',')
            .map(|field| field.trim().trim_matches('"'))
            .any(|field| ELEVATED_LEVELS.contains(&field))
    }))
}

/// The SID of the user running this, which the task runs as.
pub(super) fn user_sid(system: &mut dyn System) -> Result<String, String> {
    user_identity(system).map(|(_, sid)| sid)
}

/// The name (`machine\user`) and the SID of the user running this.
pub(crate) fn user_identity(system: &mut dyn System) -> Result<(String, String), String> {
    let cmd = Cmd::new("whoami.exe", &["/user", "/fo", "csv", "/nh"]);
    let output = run_checked(system, &cmd)?;
    // `"machine\user","S-1-5-21-..."`
    let line = output.stdout.trim();
    let (name, sid) = line.rsplit_once(',').unwrap_or(("", line));
    let name = name.trim().trim_matches('"');
    let sid = sid.trim().trim_matches('"');
    let valid = sid.strip_prefix("S-1-").is_some_and(|rest| {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '-')
    });
    if !valid {
        return Err(format!("`{cmd}` didn't print a user's SID"));
    }
    Ok((name.to_owned(), sid.to_owned()))
}

/// `word` as one argument of a Windows command line, in double quotes, as
/// the C runtime reads them.
fn quote(word: &str) -> String {
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for c in word.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        // Backslashes are literal unless a quote follows them: then each
        // is doubled, and the quote escaped.
        let count = if c == '"' {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        quoted.extend(std::iter::repeat_n('\\', count));
        quoted.push(c);
        backslashes = 0;
    }
    // The closing quote follows the last ones.
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

/// `service run`'s arguments for `definition`.
fn run_arguments(definition: &Definition, system: bool) -> String {
    let flag = if system { " -system" } else { "" };
    let mut arguments = format!("service run{flag} -config {}", quote(&definition.config));
    // `service run` goes to the config's directory unless it is told
    // another, such as the directory CLIProxyAPI ran in.
    let own = Platform::Windows.parent(&definition.config);
    if own.as_deref() != Some(definition.dir.as_str()) {
        arguments.push_str(&format!(" -dir {}", quote(&definition.dir)));
    }
    arguments
}

/// The task's definition: at the logon of the user whose SID is `sid`, as
/// them, run `definition` through `service run`.
pub(super) fn task_xml(definition: &Definition, sid: &str) -> String {
    let mut xml = String::new();
    let _ = write!(
        xml,
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>open-ferry AI proxy. Installed by `open-ferry service install`; `open-ferry service uninstall` removes it.</Description>
    <URI>\{SERVICE_NAME}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{sid}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{sid}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
      <WorkingDirectory>{dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        command = escape(&quote(&definition.exe)),
        arguments = escape(&run_arguments(definition, false)),
        dir = escape(&definition.dir),
    );
    xml
}

fn schtasks(args: &[&str]) -> Cmd {
    Cmd::new("schtasks.exe", args)
}

fn sc(args: &[&str]) -> Cmd {
    Cmd::new("sc.exe", args)
}

/// Succeeds when the task exists.
pub(super) fn query_task() -> Cmd {
    schtasks(&["/query", "/tn", SERVICE_NAME])
}

pub(super) fn task_status() -> Cmd {
    schtasks(&["/query", "/tn", SERVICE_NAME, "/fo", "list", "/v"])
}

/// Writes the task's definition to `file`, registers it, and starts it.
pub(super) fn task_install_steps(file: &str, definition: &Definition, sid: &str) -> Vec<Step> {
    vec![
        Step::Write {
            path: file.to_owned(),
            text: task_xml(definition, sid),
            utf16: true,
        },
        Step::Run(schtasks(&["/create", "/tn", SERVICE_NAME, "/xml"]).arg(file)),
        Step::Run(schtasks(&["/run", "/tn", SERVICE_NAME])),
    ]
}

/// Stops the task, which ends the server with it, and deletes it.
pub(super) fn task_uninstall_steps() -> Vec<Step> {
    vec![
        Step::TryRun(schtasks(&["/end", "/tn", SERVICE_NAME])),
        Step::Run(schtasks(&["/delete", "/tn", SERVICE_NAME, "/f"])),
    ]
}

/// Succeeds when the service exists, and says how it is.
pub(super) fn query_service() -> Cmd {
    sc(&["query", SERVICE_NAME])
}

/// The service's command line.
pub(super) fn service_command(definition: &Definition) -> String {
    format!(
        "{} {}",
        quote(&definition.exe),
        run_arguments(definition, true)
    )
}

/// Creates the service, to start at boot and again 5 seconds after any
/// failure, and starts it.
pub(super) fn service_install_steps(definition: &Definition) -> Vec<Step> {
    vec![
        Step::Run(
            sc(&["create", SERVICE_NAME, "binPath="])
                .arg(service_command(definition))
                .arg("start=")
                .arg("auto")
                .arg("DisplayName=")
                .arg(SERVICE_NAME),
        ),
        Step::Run(sc(&["description", SERVICE_NAME, "open-ferry AI proxy"])),
        Step::Run(sc(&[
            "failure",
            SERVICE_NAME,
            "reset=",
            "86400",
            "actions=",
            "restart/5000",
        ])),
        // Counts an exit with an error as a failure, not only a crash.
        Step::Run(sc(&["failureflag", SERVICE_NAME, "1"])),
        Step::Run(sc(&["start", SERVICE_NAME])),
    ]
}

/// Stops the service and deletes it.
pub(super) fn service_uninstall_steps() -> Vec<Step> {
    vec![
        Step::TryRun(sc(&["stop", SERVICE_NAME])),
        Step::Run(sc(&["delete", SERVICE_NAME])),
    ]
}

pub(super) fn task_notes(log: &str) -> String {
    format!(
        "open-ferry is installed and started, as a scheduled task: it starts as you when you log on, and again when it fails.\nIts output: {log}"
    )
}

pub(super) fn service_notes(log: &str) -> String {
    format!(
        "open-ferry is installed and started, as a Windows service: it starts at boot as LocalSystem, and again when it fails.\nIts output: {log}; the service manager logs its starts and stops in the System event log."
    )
}
