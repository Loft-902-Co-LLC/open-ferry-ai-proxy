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
//! When the binary can update itself the task runs `open-ferry service run
//! -supervise -config <path> -data-dir <path>` instead: the same wrapper,
//! which then starts the blue-green supervisor, not the server, in the job
//! that ends it with the wrapper, and fails closed (starts nothing) if it
//! can't make the job. `-data-dir` pins the data directory the install chose,
//! so the supervisor doesn't work it out again from an environment the task
//! may not share.
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

use super::launchd::{escape, unescape};
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

/// `service run`'s arguments for `definition`. A supervised task runs
/// `service run -supervise -config <path> -data-dir <path>`, and the wrapper
/// starts `open-ferry -supervise ...` as its child; a system service runs
/// the supervisor in the SCM wrapper itself.
fn run_arguments(definition: &Definition, system: bool) -> String {
    let flag = if system { " -system" } else { "" };
    let mut arguments = match &definition.data_dir {
        Some(data_dir) => format!(
            "service run{flag} -supervise -config {} -data-dir {}",
            quote(&definition.config),
            quote(data_dir)
        ),
        _ => format!("service run{flag} -config {}", quote(&definition.config)),
    };
    // `service run` goes to the config's directory unless it is told
    // another, such as the directory CLIProxyAPI ran in.
    let own = Platform::Windows.parent(&definition.config);
    if own.as_deref() != Some(definition.dir.as_str()) {
        arguments.push_str(&format!(" -dir {}", quote(&definition.dir)));
    }
    arguments
}

/// The words of a Windows command line, split as the C runtime splits it:
/// the inverse of [`quote`] for what [`run_arguments`] writes.
/// Only a space or a tab separates words, as in the C runtime; any other
/// whitespace outside double quotes is `None`.
fn split_command_line(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut backslashes = 0;
    for c in line.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Pairs of backslashes before a quote are one each, and the
                // quote then opens or closes; an odd one escapes the quote.
                word.extend(std::iter::repeat_n('\\', backslashes / 2));
                if backslashes % 2 == 1 {
                    word.push('"');
                } else {
                    quoted = !quoted;
                }
                backslashes = 0;
                started = true;
            }
            c => {
                word.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                if (c == ' ' || c == '\t') && !quoted {
                    if started {
                        words.push(std::mem::take(&mut word));
                        started = false;
                    }
                } else if c.is_whitespace() && !quoted {
                    return None;
                } else {
                    word.push(c);
                    started = true;
                }
            }
        }
    }
    if quoted {
        return None;
    }
    word.extend(std::iter::repeat_n('\\', backslashes));
    if started || backslashes > 0 {
        words.push(word);
    }
    Some(words)
}

/// The positions of the opening tags `<name` in `xml`: the name must end
/// there (`<ExecutionTimeLimit>` is not an `<Exec`).
fn opens(xml: &str, name: &str) -> Vec<usize> {
    let tag = format!("<{name}");
    xml.match_indices(&tag)
        .map(|(at, _)| at)
        .filter(|at| {
            matches!(
                xml.as_bytes().get(at + tag.len()),
                Some(b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n')
            )
        })
        .collect()
}

/// The content of the element opened at `at` (empty for `<name/>`), and
/// where it ends.
fn element<'a>(xml: &'a str, at: usize, name: &str) -> Option<(&'a str, usize)> {
    let open_end = at + xml.get(at..)?.find('>')?;
    if xml.get(at..open_end)?.ends_with('/') {
        return Some(("", open_end + 1));
    }
    let close = format!("</{name}>");
    let end = open_end + 1 + xml.get(open_end + 1..)?.find(&close)?;
    Some((xml.get(open_end + 1..end)?, end + close.len()))
}

/// The `<Command>` and the `<Arguments>` (empty when there are none) of the
/// one action of a task's XML, as written. `None` unless the XML is the
/// shape our installer writes and `schtasks /query /xml` prints back: one
/// `<Actions>` with one `<Exec>` and nothing else, one `<Command>` and at
/// most one `<Arguments>`, both inside it, and no comment or CDATA.
fn exec_parts(xml: &str) -> Option<(&str, &str)> {
    if xml.contains("<!--") || xml.contains("<![CDATA[") {
        return None;
    }
    let [actions] = opens(xml, "Actions")[..] else {
        return None;
    };
    let (body, _) = element(xml, actions, "Actions")?;
    let [exec] = opens(body, "Exec")[..] else {
        return None;
    };
    let (exec_body, exec_end) = element(body, exec, "Exec")?;
    // Any other action, of any type, makes the task something else.
    if !body.get(..exec)?.trim().is_empty() || !body.get(exec_end..)?.trim().is_empty() {
        return None;
    }
    let [command] = opens(exec_body, "Command")[..] else {
        return None;
    };
    let arguments = opens(exec_body, "Arguments");
    if opens(xml, "Command").len() != 1 || opens(xml, "Arguments").len() != arguments.len() {
        return None;
    }
    let text = |at: usize, name: &str| {
        element(exec_body, at, name)
            .map(|(text, _)| text)
            .filter(|text| !text.contains('<'))
    };
    let arguments = match arguments[..] {
        [] => "",
        [at] => text(at, "Arguments")?,
        _ => return None,
    };
    Some((text(command, "Command")?, arguments))
}

/// The words of the `<Arguments>` of a task's XML, as `schtasks /query /xml`
/// prints it.
///
/// An omitted or empty (`<Arguments/>`) element is an empty list: the task
/// was read, and it runs its command with no arguments. `None` when the XML
/// isn't plainly our task's (see `exec_parts`), or the arguments hold an
/// unknown entity or whitespace the C runtime doesn't split at.
pub(super) fn task_words(xml: &str) -> Option<Vec<String>> {
    split_command_line(&unescape(exec_parts(xml)?.1)?)
}

/// The binary a task's XML runs: its `<Command>`, which holds one path (the
/// arguments are elsewhere), so it is not split at spaces. Surrounding
/// whitespace and one pair of double quotes are dropped. Outer `None`: the
/// XML can't be read; inner `None`: the command is empty.
fn task_binary(xml: &str) -> Option<Option<String>> {
    let text = unescape(exec_parts(xml)?.0)?;
    let text = text.trim();
    let text = text
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(text);
    Some((!text.is_empty()).then(|| text.to_owned()))
}

/// [`task_binary`], with an unreadable XML the same as an empty command.
#[cfg(test)]
pub(super) fn task_command(xml: &str) -> Option<String> {
    task_binary(xml).flatten()
}

/// The words and the binary of a task's XML, or `None` when it can't be read.
pub(super) fn task_definition(xml: &str) -> Option<(Vec<String>, Option<String>)> {
    Some((task_words(xml)?, task_binary(xml)?))
}

/// Prints the task's XML.
pub(super) fn task_xml_query() -> Cmd {
    schtasks(&["/query", "/tn", SERVICE_NAME, "/xml"])
}

/// The task's definition: at the logon of the user whose SID is `sid`, as
/// them, run `definition` through `service run`.
///
/// `RestartOnFailure` (a minute apart, ten times) covers only the scheduler's
/// own failure to launch `service run`. It is not how the server is kept
/// running: `service run` starts the server, or the supervisor, again itself
/// when it fails (see `run`), and ends when that exits cleanly.
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
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>10</Count>
    </RestartOnFailure>
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

/// Lists every task, one CSV line each. Its output says a task is not
/// there where a failed query by name does not: that failure reads the same
/// for a task that is not there and for one the scheduler wouldn't say.
pub(super) fn list_tasks() -> Cmd {
    schtasks(&["/query", "/fo", "csv", "/nh"])
}

/// Whether the output of [`list_tasks`] has open-ferry's task.
pub(crate) fn task_listed(stdout: &str) -> bool {
    stdout.lines().any(|line| {
        let name = line.split(',').next().unwrap_or_default();
        let name = name.trim().trim_matches('"').trim_start_matches('\\');
        name.eq_ignore_ascii_case(SERVICE_NAME)
    })
}

/// The number on the `STATE` line of `sc query`'s output (1 is stopped, 4
/// is running), and nothing from a service's name or any other line.
pub(crate) fn service_state_number(stdout: &str) -> Option<u32> {
    stdout.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("STATE")?.trim_start();
        rest.strip_prefix(':')?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

/// `sc query`'s state number for a stopped service.
pub(crate) const SERVICE_STOPPED: u32 = 1;

/// `sc query`'s state number for a running service.
pub(crate) const SERVICE_RUNNING: u32 = 4;

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

/// The registered service's command line, as SCM reports it.
pub(super) fn service_config_query() -> Cmd {
    sc(&["qc", SERVICE_NAME])
}

pub(super) fn service_definition(output: &str) -> Option<Vec<String>> {
    let mut lines = output.lines().filter_map(|line| {
        line.trim()
            .strip_prefix("BINARY_PATH_NAME")?
            .trim_start()
            .strip_prefix(':')
    });
    let command = lines.next()?.trim();
    if lines.next().is_some() {
        return None;
    }
    let words = split_command_line(command)?;
    if words.first().is_none_or(String::is_empty) {
        return None;
    }
    Some(words)
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
            "restart/5000/restart/60000/restart/60000",
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
