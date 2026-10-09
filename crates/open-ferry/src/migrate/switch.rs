//! The switch, and its undoing.
//!
//! - **A service** (systemd, launchd, a Windows service, a scheduled task
//!   that runs the binary): CLIProxyAPI's is stopped and disabled, and its
//!   definition kept; open-ferry's is installed with `service install`'s
//!   code, with the same config and working directory, and started.
//! - **A drop-in**, for anything else: CLIProxyAPI's binary is renamed to
//!   `<name>.cliproxyapi`, and open-ferry copied under its name, so that
//!   whatever starts it starts open-ferry. With the person's yes, the
//!   process is stopped and started again with the same command line.
//! - **A container:** the steps to take are printed; nothing is changed.
//!
//! Before any change, the config, the `.env` files and the auth directory
//! are copied into a backup, and a record of the switch is written (see
//! [`record`]). After it, `migrate` waits for open-ferry to answer on the
//! config's address, and undoes the switch when it doesn't.

use std::io::Write;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};

use super::assess::{Assessment, Kind, Listen};
use super::discover::{self, Found, Launcher, Starter};
use super::machine::{Answer, EntryKind, Launch, Machine, Proc};
use super::record::{self, Address, Backup, Before, Copied, Record, Status, Switch, Theirs};
use crate::os_service::{self, Cmd, Context, Platform, Target, run_checked, say};

/// How long open-ferry has to answer after the switch.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the address is asked.
const POLL: Duration = Duration::from_millis(500);

/// How long a stopped service's process has to exit.
const EXIT_WAIT: Duration = Duration::from_secs(20);

/// How long a launcher has to start the binary again after it is stopped,
/// before `migrate` starts it.
const LAUNCHER_WAIT: Duration = Duration::from_secs(3);

/// How long CLIProxyAPI has to answer again after the switch is undone.
const BACK_WAIT: Duration = Duration::from_secs(15);

/// What CLIProxyAPI's binary is renamed with.
pub(crate) const MOVED_SUFFIX: &str = ".cliproxyapi";

/// The Docker image open-ferry publishes.
const IMAGE: &str = "ghcr.io/loft-902-co-llc/open-ferry:latest";

/// The guide the plan and the docs point to.
const GUIDE: &str = "docs/migrating-from-cliproxyapi.md";

/// What the switch would do.
#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub(crate) kind: Kind,
    /// The steps, in words, in order.
    pub(crate) steps: Vec<String>,
    pub(crate) blockers: Vec<String>,
    pub(crate) good_to_know: Vec<String>,
    pub(crate) backup_dir: Option<String>,
    /// Where the record goes, or why it can't.
    pub(crate) record: Result<String, String>,
    /// The command that makes the switch.
    pub(crate) command: String,
    /// CLIProxyAPI's service, for a service switch.
    pub(crate) theirs: Option<Theirs>,
}

/// The time as the record and the messages write it.
pub(crate) fn timestamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The backup's directory: `open-ferry-migrate-<UTC time>` beside the
/// config, or beside the auth directory when the config is inside it.
pub(crate) fn backup_dir(
    platform: Platform,
    config: &str,
    auth_dir: Option<&str>,
    now: DateTime<Utc>,
) -> Option<String> {
    let name = format!("open-ferry-migrate-{}", now.format("%Y%m%dT%H%M%SZ"));
    let mut dir = platform.parent(config)?;
    if let Some(auth) = auth_dir
        && platform.is_within(&dir, auth)
    {
        dir = platform.parent(auth)?;
    }
    Some(platform.join(&dir, &name))
}

/// CLIProxyAPI's service, as the record keeps it.
pub(crate) fn theirs_of(starter: &Starter, running: bool) -> Option<Theirs> {
    Some(match starter {
        Starter::Systemd {
            unit,
            user,
            enabled,
            active,
        } => Theirs::Systemd {
            unit: unit.clone(),
            user: *user,
            was_enabled: *enabled,
            was_active: *active,
        },
        Starter::Launchd {
            label,
            plist,
            domain,
            loaded,
            ..
        } => Theirs::Launchd {
            label: label.clone(),
            plist: plist.clone(),
            domain: domain.clone(),
            was_loaded: *loaded,
        },
        Starter::WindowsService {
            name,
            start,
            running,
            ..
        } => Theirs::WindowsService {
            name: name.clone(),
            start_type: start.clone(),
            was_running: *running,
        },
        Starter::Task { name, enabled } => Theirs::Task {
            name: name.clone(),
            was_enabled: *enabled,
            was_running: running,
        },
        Starter::Launcher(_) | Starter::Container(_) => return None,
    })
}

/// The commands that stop and disable CLIProxyAPI's service, keeping its
/// definition.
pub(crate) fn stop_commands(theirs: &Theirs) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    match theirs {
        Theirs::Systemd {
            unit,
            user,
            was_enabled,
            was_active,
        } => {
            if *was_active {
                cmds.push(discover::systemctl(*user, &["stop", unit]));
            }
            if *was_enabled {
                cmds.push(discover::systemctl(*user, &["disable", unit]));
            }
        }
        Theirs::Launchd {
            label,
            domain,
            was_loaded,
            ..
        } => {
            let service = format!("{domain}/{label}");
            if *was_loaded {
                cmds.push(Cmd::new("launchctl", &["bootout"]).arg(service.clone()));
            }
            cmds.push(Cmd::new("launchctl", &["disable"]).arg(service));
        }
        Theirs::WindowsService {
            name,
            start_type,
            was_running,
        } => {
            if *was_running {
                cmds.push(Cmd::new("sc.exe", &["stop", name]));
            }
            if start_type != "disabled" {
                cmds.push(Cmd::new("sc.exe", &["config", name, "start=", "disabled"]));
            }
        }
        Theirs::Task {
            name, was_enabled, ..
        } => {
            if *was_enabled {
                cmds.push(Cmd::new(
                    "schtasks.exe",
                    &["/change", "/tn", name, "/disable"],
                ));
            }
        }
    }
    cmds
}

/// Whether CLIProxyAPI's service ran before the switch, so that turning
/// it back on starts it.
pub(crate) fn was_running(theirs: &Theirs) -> bool {
    match theirs {
        Theirs::Systemd { was_active, .. } => *was_active,
        Theirs::Launchd { was_loaded, .. } => *was_loaded,
        Theirs::WindowsService { was_running, .. } | Theirs::Task { was_running, .. } => {
            *was_running
        }
    }
}

/// The commands that put CLIProxyAPI's service back as it was.
pub(crate) fn start_commands(theirs: &Theirs) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    match theirs {
        Theirs::Systemd {
            unit,
            user,
            was_enabled,
            was_active,
        } => {
            if *was_enabled {
                cmds.push(discover::systemctl(*user, &["enable", unit]));
            }
            if *was_active {
                cmds.push(discover::systemctl(*user, &["start", unit]));
            }
        }
        Theirs::Launchd {
            label,
            plist,
            domain,
            was_loaded,
        } => {
            cmds.push(Cmd::new("launchctl", &["enable"]).arg(format!("{domain}/{label}")));
            if *was_loaded {
                cmds.push(Cmd::new("launchctl", &["bootstrap", domain, plist]));
            }
        }
        Theirs::WindowsService {
            name,
            start_type,
            was_running,
        } => {
            if start_type != "disabled" {
                cmds.push(Cmd::new("sc.exe", &["config", name, "start=", start_type]));
            }
            if *was_running {
                cmds.push(Cmd::new("sc.exe", &["start", name]));
            }
        }
        Theirs::Task {
            name,
            was_enabled,
            was_running,
        } => {
            if *was_enabled {
                cmds.push(Cmd::new(
                    "schtasks.exe",
                    &["/change", "/tn", name, "/enable"],
                ));
            }
            if *was_running {
                cmds.push(Cmd::new("schtasks.exe", &["/run", "/tn", name]));
            }
        }
    }
    cmds
}

/// Commands, as the plan lists them.
fn listed(cmds: &[Cmd]) -> String {
    let cmds: Vec<String> = cmds.iter().map(|cmd| format!("`{cmd}`")).collect();
    cmds.join(", ")
}

/// What `service install`'s plan does, in a few words.
fn summarize(plan: &os_service::Plan) -> String {
    let mut done = Vec::new();
    for step in &plan.steps {
        done.push(match step {
            os_service::Step::CreateDir(dir) => format!("create {dir}"),
            os_service::Step::Write { path, .. } => format!("write {path}"),
            os_service::Step::Remove(path) => format!("remove {path}"),
            os_service::Step::Run(cmd) | os_service::Step::TryRun(cmd) => format!("run `{cmd}`"),
        });
    }
    if !plan.cleanup.is_empty() {
        done.push(format!("remove {} afterwards", plan.cleanup.join(", ")));
    }
    done.join("; ")
}

/// The address the switch checks, for messages.
pub(crate) fn address(listen: &Listen) -> String {
    let scheme = if listen.tls { "https" } else { "http" };
    match listen.probe {
        Some(ip) => format!("{scheme}://{}", std::net::SocketAddr::new(ip, listen.port)),
        None => format!("{scheme}://{}:{}", listen.host, listen.port),
    }
}

/// Why a switch of a system service needs more rights, and the command to
/// run instead.
fn needs_privilege(platform: Platform, command: &str) -> String {
    match platform {
        Platform::Windows => format!(
            "Switching a Windows service needs an administrator. Run this in a terminal opened with \"Run as administrator\": {command}"
        ),
        Platform::Linux | Platform::MacOs => {
            format!("Switching a system service needs root. Run: {command}")
        }
    }
}

/// Makes the plan, changing nothing. `given` is `migrate`'s `-config`.
pub(crate) fn plan(
    machine: &mut dyn Machine,
    context: &Context,
    found: &Found,
    assessment: &Assessment,
    kind: Kind,
    given: Option<&str>,
) -> Plan {
    let platform = context.platform;
    let mut command = format!("{} {}", platform.quote(&context.exe), super::NAME);
    if let Some(config) = given {
        command.push_str(&format!(" -config {}", platform.quote(config)));
    }
    let record_path = record::path(context);
    let mut plan = Plan {
        kind,
        steps: Vec::new(),
        blockers: Vec::new(),
        good_to_know: Vec::new(),
        backup_dir: assessment.config.as_deref().and_then(|config| {
            backup_dir(
                platform,
                config,
                assessment.auth_dir.as_deref(),
                machine.now(),
            )
        }),
        record: record_path.clone(),
        command,
        theirs: theirs_of(&found.starter, found.process.is_some()),
    };
    match &record_path {
        Ok(path) => match record::load(machine, path) {
            Ok(Some(existing))
                if matches!(existing.status, Status::Switched | Status::Switching) =>
            {
                plan.blockers.push(format!(
                    "A switch made at {} isn't undone ({path}). Run `open-ferry {} -undo` before switching again.",
                    existing.created,
                    super::NAME
                ));
            }
            Ok(_) => {}
            Err(error) => plan.blockers.push(error),
        },
        Err(error) if kind != Kind::Container => plan.blockers.push(error.clone()),
        Err(_) => {}
    }
    let backup = match &plan.backup_dir {
        Some(dir) => {
            let private = match platform {
                Platform::Windows => "",
                Platform::Linux | Platform::MacOs => ", which only you can open",
            };
            format!(
                "Back up the config, its .env files and the auth directory into {dir}{private}."
            )
        }
        None => "Back up the config, its .env files and the auth directory.".to_owned(),
    };
    let record_step = match &plan.record {
        Ok(path) => format!(
            "Write the switch's record to {path}, with a copy in the backup, for `open-ferry {} -undo`.",
            super::NAME
        ),
        Err(_) => "Write the switch's record.".to_owned(),
    };
    let verify = match &assessment.listen {
        Some(listen) if listen.probe.is_some() => format!(
            "Wait up to {} seconds for open-ferry to answer on {}, and undo the switch if it doesn't.",
            VERIFY_TIMEOUT.as_secs(),
            address(listen)
        ),
        _ => {
            "Start it; its address can't be checked, so check it yourself with `open-ferry check`."
                .to_owned()
        }
    };
    match kind {
        Kind::Service(target) => {
            plan.steps.push(backup);
            plan.steps.push(record_step);
            service_plan(machine, context, found, assessment, target, &mut plan);
            plan.steps.push(verify);
        }
        Kind::DropIn => {
            plan.steps.push(backup);
            plan.steps.push(record_step);
            drop_in_plan(machine, context, found, assessment, &mut plan);
            plan.steps.push(verify);
        }
        Kind::Container => container_plan(found, &mut plan),
    }
    if kind != Kind::Container {
        plan.good_to_know.push(format!(
            "To switch back: `open-ferry {} -undo`. It leaves the config and the credentials as they are; with -restore it copies the backed-up ones back.",
            super::NAME
        ));
        plan.good_to_know.push(
            "If you switch back and CLIProxyAPI saves the config (from its management panel or API), it comments out the sections only open-ferry reads, `routing.quota` and `management.separate-address`.".to_owned(),
        );
        plan.good_to_know.push(
            "Don't run CLIProxyAPI and open-ferry on the same auth directory at once: each refreshes tokens on its own, and a refresh token one has used may be refused to the other.".to_owned(),
        );
    }
    plan
}

fn service_plan(
    machine: &mut dyn Machine,
    context: &Context,
    found: &Found,
    assessment: &Assessment,
    target: Target,
    plan: &mut Plan,
) {
    let platform = context.platform;
    let Some(theirs) = plan.theirs.clone() else {
        return;
    };
    let stop = stop_commands(&theirs);
    let what = found.starter.describe();
    if let Starter::Launchd { brew: true, .. } = &found.starter {
        plan.good_to_know.push(
            "`brew services start cliproxyapi` and `brew services restart cliproxyapi` load CLIProxyAPI's job again, beside open-ferry's and on the same port: switch back with `open-ferry migrate -undo` instead.".to_owned(),
        );
    }
    let mut text = match &theirs {
        Theirs::Task { .. } => format!("Disable {what}, keeping it"),
        _ => format!("Stop and disable {what}, keeping its definition"),
    };
    if !stop.is_empty() {
        text.push_str(&format!(": {}", listed(&stop)));
    }
    if let Some(process) = &found.process {
        match &theirs {
            Theirs::Task { .. } => text.push_str(&format!(
                ". Then stop CLIProxyAPI (process {}){}",
                process.pid,
                if platform == Platform::Windows {
                    ", which Windows ends at once"
                } else {
                    ""
                }
            )),
            _ => text.push_str(&format!(
                ". Wait up to {} seconds for CLIProxyAPI (process {}) to exit",
                EXIT_WAIT.as_secs(),
                process.pid
            )),
        }
    }
    text.push('.');
    plan.steps.push(text);
    let (Some(config), true) = (assessment.config.as_deref(), assessment.loaded) else {
        return;
    };
    match os_service::prepare_install(machine, context, target, config, found.cwd.as_deref(), true)
    {
        Ok(prepared) => {
            if target.system() && !prepared.account.privileged() {
                let command = match platform {
                    Platform::Windows => plan.command.clone(),
                    Platform::Linux | Platform::MacOs => format!("sudo {}", plan.command),
                };
                plan.blockers.push(needs_privilege(platform, &command));
                plan.command = command;
            }
            let location = target.location(context).unwrap_or_default();
            plan.steps.push(format!(
                "Install open-ferry as {} ({location}), running `{} -config {}` in {}, and start it: {}.",
                target.kind(),
                prepared.definition.exe,
                prepared.definition.config,
                prepared.definition.dir,
                summarize(&prepared.plan)
            ));
        }
        Err(error) => plan.blockers.push(error),
    }
}

fn drop_in_plan(
    machine: &mut dyn Machine,
    context: &Context,
    found: &Found,
    assessment: &Assessment,
    plan: &mut Plan,
) {
    let platform = context.platform;
    let Starter::Launcher(launcher) = &found.starter else {
        return;
    };
    let Some(process) = &found.process else {
        // The search may have said so already, naming the launcher.
        if found.blockers.is_empty() {
            plan.blockers.push(
                "CLIProxyAPI isn't running, and what starts it isn't a service open-ferry can switch. Start it, then run `open-ferry migrate` again.".to_owned(),
            );
        }
        return;
    };
    let Some(exe) = &found.exe else {
        plan.blockers.push(format!(
            "The path of CLIProxyAPI's binary (process {}) can't be read.",
            process.pid
        ));
        return;
    };
    let moved = format!("{exe}{MOVED_SUFFIX}");
    if machine.exists(&moved) {
        plan.blockers.push(format!(
            "{moved} already exists, so CLIProxyAPI's binary can't be moved there. If it is left from an earlier switch, put it back or remove it first."
        ));
    }
    if discover::same_path(platform, exe, &context.exe) {
        plan.blockers.push(format!(
            "{exe} is this open-ferry binary: run the open-ferry you installed, not one in CLIProxyAPI's place."
        ));
    }
    plan.steps.push(format!(
        "Rename CLIProxyAPI's binary, {exe}, to {moved}, and copy open-ferry ({}) to {exe}, so that {} starts open-ferry.",
        context.exe,
        launcher_name(launcher)
    ));
    let cwd = process
        .cwd
        .clone()
        .or_else(|| platform.parent(exe))
        .unwrap_or_default();
    let log = plan
        .backup_dir
        .as_deref()
        .map(|dir| platform.join(dir, "open-ferry.log"))
        .unwrap_or_default();
    let how = match platform {
        Platform::Windows => "ended (Windows ends it at once)",
        Platform::Linux | Platform::MacOs => {
            "asked to stop, as CLIProxyAPI stops gracefully on SIGTERM"
        }
    };
    plan.steps.push(format!(
        "Ask you, unless -yes is given, whether to stop CLIProxyAPI (process {pid}) now. With your yes, it is {how}; if {launcher} doesn't start it again within {wait} seconds, open-ferry is started in its place with the same command line, in {cwd}, its output going to {log}. If not, {hint}, and open-ferry starts in its place.",
        pid = process.pid,
        launcher = launcher_name(launcher),
        wait = LAUNCHER_WAIT.as_secs(),
        hint = launcher.restart_hint(),
    ));
    plan.good_to_know.push(
        "Anything that updates or reinstalls CLIProxyAPI's binary in place puts CLIProxyAPI back over open-ferry: a launcher's or tray app's updater (such as EasyCLIProxyAPI's), a package manager (Homebrew, Scoop, the AUR), or an install script. Turn its updates off, or run `open-ferry migrate` again after one.".to_owned(),
    );
    if discover::brew_prefix(exe).is_some() {
        plan.good_to_know.push(
            "Homebrew installed this binary: `brew upgrade` and `brew reinstall` put CLIProxyAPI back.".to_owned(),
        );
    }
    plan.good_to_know.push(format!(
        "open-ferry runs under CLIProxyAPI's file name, and takes its command line: the flags it doesn't take are listed above. To tell them apart, `{}` in its place logs `open-ferry Version: ...` as it starts, and answers `GET /` with `open-ferry-ai-proxy`.",
        discover::file_name(platform, exe)
    ));
    if assessment.flags.rest {
        plan.good_to_know.push(
            "Its command line has arguments after the flags, which open-ferry, as CLIProxyAPI, ignores.".to_owned(),
        );
    }
}

/// The launcher, in a few words, for the plan.
fn launcher_name(launcher: &Launcher) -> String {
    match launcher {
        Launcher::Task { name, .. } => format!("the scheduled task {name}"),
        Launcher::Unit { unit } => format!("the systemd unit {unit}"),
        Launcher::Job { label, .. } => format!("the launchd job {label}"),
        Launcher::Program { name, .. } => format!("the program {name}"),
        Launcher::Gone => "what started it".to_owned(),
    }
}

fn container_plan(found: &Found, plan: &mut Plan) {
    let Starter::Container(container) = &found.starter else {
        return;
    };
    let mut place = String::from("In the Compose file");
    if let Some(files) = &container.files {
        place = format!("In {files}");
    } else if let Some(dir) = &container.working_dir {
        place = format!("In the Compose file in {dir}");
    }
    if let Some(service) = &container.service {
        place.push_str(&format!(", for the service {service}"));
    }
    plan.steps = vec![
        format!(
            "{place}, change the image to {IMAGE}, or to a version. If the file takes the image from CLI_PROXY_IMAGE, set that variable instead."
        ),
        "Remove the build: section, which builds CLIProxyAPI from a checkout.".to_owned(),
        "Remove the sign-in callback ports (8085, 1455, 54545, 51121 and 11451); keep 8317.".to_owned(),
        "Remove the plugins volume and DEPLOY: neither plugins nor the cloud deploy mode are ported.".to_owned(),
        "Consider mounting the config's directory rather than the file, if you save the config from the dashboard or the management API.".to_owned(),
        "Set TZ if you want local times in the logs: CLIProxyAPI's image sets Asia/Shanghai, open-ferry's is in UTC.".to_owned(),
        match &container.working_dir {
            Some(dir) => format!("Run `docker compose up -d` in {dir}."),
            None => "Run `docker compose up -d`.".to_owned(),
        },
    ];
    plan.good_to_know.push(format!(
        "open-ferry doesn't change containers or Compose files: these are the steps of {GUIDE}, under Docker Compose. The auth directory and log mounts stay as they are."
    ));
    plan.command = format!("docker compose up -d (after the changes above; see {GUIDE})");
}

/// A record of the switch, as it begins.
fn new_record(
    machine: &dyn Machine,
    context: &Context,
    found: &Found,
    assessment: &Assessment,
    config: &str,
    backup: Backup,
    switch: Switch,
) -> Record {
    let now = timestamp(machine.now());
    Record {
        version: record::VERSION,
        created: now.clone(),
        updated: now,
        status: Status::Switching,
        platform: record::platform_name(context.platform).to_owned(),
        cliproxyapi: Before {
            exe: found.exe.clone(),
            config: config.to_owned(),
            working_dir: assessment.working_dir.clone(),
            auth_dir: assessment.auth_dir.clone(),
            listen: assessment.listen.as_ref().map(|listen| Address {
                host: listen.host.clone(),
                port: listen.port,
                tls: listen.tls,
            }),
            started_by: found.starter.describe(),
        },
        backup: Some(backup),
        switch,
    }
}

/// Saves `record` with `status`; a failure is said, not returned: the
/// switch itself is done or undone by then.
fn finish(
    machine: &mut dyn Machine,
    platform: Platform,
    path: &str,
    record: &mut Record,
    status: Status,
    out: &mut dyn Write,
) {
    record.status = status;
    record.updated = timestamp(machine.now());
    if let Err(error) = record::save(machine, platform, path, record) {
        say(out, format_args!("{error}"));
    }
}

/// Copies `from`, a directory, to `to`, with what it holds, but for
/// symbolic links, `skip`, and at the top a `logs` directory.
fn copy_tree(
    machine: &mut dyn Machine,
    platform: Platform,
    from: &str,
    to: &str,
    skip: &str,
    top: bool,
) -> Result<(), String> {
    if !machine.exists(to) {
        machine
            .create_dir_all(to)
            .map_err(|error| format!("failed to create {to}: {error}"))?;
    }
    let entries = machine
        .list_dir(from)
        .map_err(|error| format!("failed to read {from}: {error}"))?;
    for entry in entries {
        let source = platform.join(from, &entry.name);
        if !skip.is_empty() && discover::same_path(platform, &source, skip) {
            continue;
        }
        let target = platform.join(to, &entry.name);
        match entry.kind {
            EntryKind::File => machine
                .copy(&source, &target)
                .map_err(|error| format!("failed to copy {source} to {target}: {error}"))?,
            EntryKind::Dir if !(top && entry.name == "logs") => {
                copy_tree(machine, platform, &source, &target, skip, false)?;
            }
            EntryKind::Dir | EntryKind::Other => {}
        }
    }
    Ok(())
}

/// Copies the file `from` to `to`, for the backup.
fn copy_file(machine: &mut dyn Machine, from: &str, to: String) -> Result<Copied, String> {
    machine
        .copy(from, &to)
        .map_err(|error| format!("failed to copy {from} to {to}: {error}"))?;
    Ok(Copied {
        from: from.to_owned(),
        to,
        dir: false,
    })
}

/// Copies the config, its `.env` files and the auth directory into `dir`,
/// which is made private.
fn back_up(
    machine: &mut dyn Machine,
    platform: Platform,
    assessment: &Assessment,
    config: &str,
    dir: &str,
    out: &mut dyn Write,
) -> Result<Backup, String> {
    machine
        .create_private_dir(dir)
        .map_err(|error| format!("failed to create the backup, {dir}: {error}"))?;
    let mut files = Vec::new();
    files.push(copy_file(
        machine,
        config,
        platform.join(dir, &discover::file_name(platform, config)),
    )?);
    let config_dir = platform.parent(config);
    for file in &assessment.env_files {
        let beside = platform
            .parent(file)
            .zip(config_dir.as_deref())
            .is_some_and(|(parent, config_dir)| discover::same_path(platform, &parent, config_dir));
        let name = if beside { ".env" } else { "working-dir.env" };
        files.push(copy_file(machine, file, platform.join(dir, name))?);
    }
    if let Some(auth) = &assessment.auth_dir
        && machine.exists(auth)
    {
        let to = platform.join(dir, "auth");
        copy_tree(machine, platform, auth, &to, dir, true)?;
        files.push(Copied {
            from: auth.clone(),
            to,
            dir: true,
        });
    }
    say(
        out,
        format_args!("Backed up the config, its .env files and the auth directory into {dir}"),
    );
    Ok(Backup {
        dir: dir.to_owned(),
        files,
    })
}

/// Copies the backed-up files back.
fn restore(
    machine: &mut dyn Machine,
    platform: Platform,
    backup: &Backup,
    out: &mut dyn Write,
) -> Result<(), String> {
    for copied in &backup.files {
        if copied.dir {
            copy_tree(machine, platform, &copied.to, &copied.from, "", false)?;
        } else {
            machine.copy(&copied.to, &copied.from).map_err(|error| {
                format!("failed to copy {} to {}: {error}", copied.to, copied.from)
            })?;
        }
        say(
            out,
            format_args!("Restored {} from {}", copied.from, copied.to),
        );
    }
    Ok(())
}

/// How waiting for open-ferry went.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verified {
    Yes,
    /// The address couldn't be asked, and why.
    Unchecked(String),
}

/// Waits for open-ferry to answer where the config says it listens.
fn wait_for_open_ferry(
    machine: &mut dyn Machine,
    listen: Option<&Listen>,
    timeout: Duration,
) -> Result<Verified, String> {
    let Some(listen) = listen else {
        return Ok(Verified::Unchecked(
            "the config's address isn't known".to_owned(),
        ));
    };
    let Some(ip) = listen.probe else {
        return Ok(Verified::Unchecked(format!(
            "{} is a host name",
            listen.host
        )));
    };
    let tries = (timeout.as_millis() / POLL.as_millis()).max(1);
    let mut last = Answer::Nothing("it wasn't asked".to_owned());
    for attempt in 0..tries {
        if attempt > 0 {
            machine.sleep(POLL);
        }
        last = machine.probe(ip, listen.port, listen.tls);
        if last == Answer::OpenFerry {
            return Ok(Verified::Yes);
        }
    }
    Err(match last {
        Answer::Other(what) => format!("something else answers on {}: {what}", address(listen)),
        Answer::Nothing(why) => format!(
            "nothing answered on {} within {} seconds ({why})",
            address(listen),
            timeout.as_secs()
        ),
        Answer::OpenFerry => "open-ferry answered".to_owned(),
    })
}

/// Waits for anything to answer on the address, for CLIProxyAPI after the
/// switch is undone; says what it found.
fn wait_for_any(
    machine: &mut dyn Machine,
    listen: Option<&Listen>,
    timeout: Duration,
) -> Option<Answer> {
    let listen = listen?;
    let ip = listen.probe?;
    let tries = (timeout.as_millis() / POLL.as_millis()).max(1);
    let mut last = None;
    for attempt in 0..tries {
        if attempt > 0 {
            machine.sleep(POLL);
        }
        let answer = machine.probe(ip, listen.port, listen.tls);
        if !matches!(answer, Answer::Nothing(_)) {
            return Some(answer);
        }
        last = Some(answer);
    }
    last
}

fn say_back(machine: &mut dyn Machine, listen: Option<&Listen>, out: &mut dyn Write) {
    let Some(shown) = listen.map(address) else {
        return;
    };
    match wait_for_any(machine, listen, BACK_WAIT) {
        Some(Answer::Other(_)) => say(out, format_args!("CLIProxyAPI answers on {shown} again.")),
        Some(Answer::OpenFerry) => say(
            out,
            format_args!("open-ferry still answers on {shown}: check what runs there."),
        ),
        Some(Answer::Nothing(_)) => say(
            out,
            format_args!("Nothing answers on {shown} yet: check that CLIProxyAPI started."),
        ),
        None => {}
    }
}

/// Waits for process `pid` to exit.
fn wait_exit(machine: &mut dyn Machine, pid: u32, timeout: Duration) -> bool {
    let tries = timeout.as_millis() / POLL.as_millis();
    for attempt in 0..=tries {
        if !machine.running(pid) {
            return true;
        }
        if attempt < tries {
            machine.sleep(POLL);
        }
    }
    false
}

/// Runs `cmds`, saying each; the failures are returned, and don't stop the
/// rest.
fn run_all(machine: &mut dyn Machine, cmds: &[Cmd], out: &mut dyn Write) -> Vec<String> {
    let mut failures = Vec::new();
    for cmd in cmds {
        match run_checked(machine, cmd) {
            Ok(_) => say(out, format_args!("Ran: {cmd}")),
            Err(error) => {
                say(out, format_args!("{error}"));
                failures.push(error);
            }
        }
    }
    failures
}

/// Makes the switch `plan` describes. `yes` is `-yes`.
pub(crate) fn execute(
    machine: &mut dyn Machine,
    context: &Context,
    found: &Found,
    assessment: &Assessment,
    plan: &Plan,
    yes: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    let config = assessment
        .config
        .clone()
        .ok_or("the config isn't known, so nothing was changed")?;
    let record_path = plan.record.clone()?;
    let backup_dir = plan
        .backup_dir
        .clone()
        .ok_or("there is nowhere to put the backup, so nothing was changed")?;
    if let Some(listen) = &assessment.listen
        && let Some(ip) = listen.probe
        && machine.probe(ip, listen.port, listen.tls) == Answer::OpenFerry
    {
        return Err(format!(
            "open-ferry already answers on {}, so nothing was changed. Is CLIProxyAPI already switched?",
            address(listen)
        ));
    }
    let base = Base {
        context,
        found,
        assessment,
        config: &config,
        record_path: &record_path,
        backup_dir: &backup_dir,
    };
    match plan.kind {
        Kind::Service(target) => execute_service(machine, &base, target, plan, out),
        Kind::DropIn => execute_drop_in(machine, &base, yes, out),
        Kind::Container => Err("open-ferry doesn't switch containers".to_owned()),
    }
}

/// What every switch needs.
struct Base<'a> {
    context: &'a Context,
    found: &'a Found,
    assessment: &'a Assessment,
    config: &'a str,
    record_path: &'a str,
    backup_dir: &'a str,
}

fn execute_service(
    machine: &mut dyn Machine,
    base: &Base<'_>,
    target: Target,
    plan: &Plan,
    out: &mut dyn Write,
) -> Result<(), String> {
    let context = base.context;
    let platform = context.platform;
    let theirs = plan
        .theirs
        .clone()
        .ok_or("CLIProxyAPI's service isn't known, so nothing was changed")?;
    // Everything that can refuse refuses before a change.
    let prepared = os_service::prepare_install(
        machine,
        context,
        target,
        base.config,
        base.found.cwd.as_deref(),
        false,
    )
    .map_err(|error| format!("{error}\nNothing was changed."))?;
    let backup = back_up(
        machine,
        platform,
        base.assessment,
        base.config,
        base.backup_dir,
        out,
    )?;
    let mut record = new_record(
        machine,
        context,
        base.found,
        base.assessment,
        base.config,
        backup,
        Switch::Service {
            theirs: theirs.clone(),
            ours: record::target_name(target).to_owned(),
        },
    );
    record::save(machine, platform, base.record_path, &record)
        .map_err(|error| format!("{error}\nNothing but the backup was made."))?;
    let listen = base.assessment.listen.as_ref();

    // CLIProxyAPI's service off.
    if let Err(error) = stop_theirs(machine, platform, &theirs, base.found, out) {
        say(
            out,
            format_args!("Failed to stop CLIProxyAPI: {error}. Turning its service back on."),
        );
        let failures = run_all(machine, &start_commands(&theirs), out);
        finish(
            machine,
            platform,
            base.record_path,
            &mut record,
            Status::RolledBack,
            out,
        );
        return Err(undone_message(
            &format!("CLIProxyAPI didn't stop: {error}"),
            &failures,
        ));
    }

    // open-ferry's on.
    if let Err(failed) = os_service::apply(machine, &prepared.plan, out) {
        say(
            out,
            format_args!(
                "Failed to install open-ferry's service: {}. Undoing the switch.",
                failed.message
            ),
        );
        let mut failures = Vec::new();
        if failed.changed
            && let Err(error) = os_service::uninstall(machine, context, target, false, out)
        {
            failures.push(error);
        }
        failures.extend(run_all(machine, &start_commands(&theirs), out));
        if was_running(&theirs) {
            say_back(machine, listen, out);
        }
        finish(
            machine,
            platform,
            base.record_path,
            &mut record,
            Status::RolledBack,
            out,
        );
        return Err(undone_message(
            &format!("open-ferry's service didn't install: {}", failed.message),
            &failures,
        ));
    }

    match wait_for_open_ferry(machine, listen, VERIFY_TIMEOUT) {
        Ok(verified) => {
            finish(
                machine,
                platform,
                base.record_path,
                &mut record,
                Status::Switched,
                out,
            );
            say(
                out,
                format_args!(
                    "{}",
                    os_service::installed_notes(context, target, &prepared.definition)
                ),
            );
            switched(out, &verified, listen, base.record_path);
            Ok(())
        }
        Err(why) => {
            say(
                out,
                format_args!("open-ferry didn't answer: {why}. Undoing the switch."),
            );
            let mut failures = Vec::new();
            if let Err(error) = os_service::uninstall(machine, context, target, false, out) {
                failures.push(error);
            }
            failures.extend(run_all(machine, &start_commands(&theirs), out));
            if was_running(&theirs) {
                say_back(machine, listen, out);
            }
            finish(
                machine,
                platform,
                base.record_path,
                &mut record,
                Status::RolledBack,
                out,
            );
            let logs = match target {
                Target::SystemdUser => ". Its logs: journalctl --user -u open-ferry".to_owned(),
                Target::SystemdSystem => ". Its logs: journalctl -u open-ferry".to_owned(),
                _ => String::new(),
            };
            Err(undone_message(
                &format!("open-ferry didn't answer: {why}{logs}"),
                &failures,
            ))
        }
    }
}

/// What to say once the switch is done.
fn switched(out: &mut dyn Write, verified: &Verified, listen: Option<&Listen>, record_path: &str) {
    match (verified, listen) {
        (Verified::Yes, Some(listen)) => say(
            out,
            format_args!("Switched: open-ferry answers on {}.", address(listen)),
        ),
        (Verified::Unchecked(why), _) => say(
            out,
            format_args!(
                "Switched, but open-ferry's answer can't be checked: {why}. Check it with `open-ferry check`."
            ),
        ),
        (Verified::Yes, None) => say(out, format_args!("Switched.")),
    }
    say(
        out,
        format_args!(
            "The switch's record is {record_path}. To switch back: open-ferry {} -undo",
            super::NAME
        ),
    );
}

/// The message of a switch that was undone.
fn undone_message(why: &str, failures: &[String]) -> String {
    if failures.is_empty() {
        format!("The switch is undone: {why}. CLIProxyAPI is back as it was; the backup is kept.")
    } else {
        format!(
            "The switch is undone as far as it could be: {why}. Undoing it failed at: {}. See {GUIDE} to finish by hand.",
            failures.join("; ")
        )
    }
}

/// Stops and disables CLIProxyAPI's service, and waits for its process
/// to exit.
fn stop_theirs(
    machine: &mut dyn Machine,
    platform: Platform,
    theirs: &Theirs,
    found: &Found,
    out: &mut dyn Write,
) -> Result<(), String> {
    for cmd in stop_commands(theirs) {
        run_checked(machine, &cmd)?;
        say(out, format_args!("Ran: {cmd}"));
    }
    let Some(process) = &found.process else {
        return Ok(());
    };
    if let Theirs::Task { .. } = theirs
        && machine.running(process.pid)
    {
        machine
            .stop(process.pid, &process.name)
            .map_err(|error| format!("failed to stop process {}: {error}", process.pid))?;
        let how = match platform {
            Platform::Windows => "ended",
            Platform::Linux | Platform::MacOs => "stopped",
        };
        say(
            out,
            format_args!("CLIProxyAPI (process {}) {how}", process.pid),
        );
    }
    if !wait_exit(machine, process.pid, EXIT_WAIT) {
        return Err(format!(
            "process {} still runs {} seconds after its service was stopped",
            process.pid,
            EXIT_WAIT.as_secs()
        ));
    }
    Ok(())
}

/// Where a binary that `migrate` takes out of the way goes: `<binary><suffix>`,
/// or with the time when that is taken.
fn aside(machine: &dyn Machine, binary: &str, suffix: &str) -> String {
    let plain = format!("{binary}{suffix}");
    if machine.exists(&plain) {
        format!("{plain}-{}", machine.now().format("%Y%m%dT%H%M%SZ"))
    } else {
        plain
    }
}

/// The processes running `binary` or `other`, by its file name.
fn running_binary(
    machine: &mut dyn Machine,
    platform: Platform,
    binary: &str,
    other: &str,
) -> Vec<Proc> {
    let name = discover::file_name(platform, binary);
    machine
        .processes(&[name.as_str()])
        .unwrap_or_default()
        .into_iter()
        .filter(|process| {
            process.exe.as_deref().is_some_and(|exe| {
                // Linux names a replaced binary `<path> (deleted)`.
                let exe = exe.strip_suffix(" (deleted)").unwrap_or(exe);
                discover::same_path(platform, exe, binary)
                    || discover::same_path(platform, exe, other)
            })
        })
        .collect()
}

/// Whether the file at `path` is this open-ferry's binary.
fn is_ours(machine: &dyn Machine, context: &Context, path: &str) -> bool {
    match (machine.read(path), machine.read(&context.exe)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Puts CLIProxyAPI's binary back at `binary` from `moved`, moving what is
/// there aside, and stops what runs it, but for the process `keep`,
/// CLIProxyAPI's own; gives what it stopped.
fn put_back_binary(
    machine: &mut dyn Machine,
    context: &Context,
    binary: &str,
    moved: &str,
    keep: Option<u32>,
    out: &mut dyn Write,
) -> Result<Vec<Proc>, String> {
    let platform = context.platform;
    let replaced = aside(machine, binary, ".open-ferry");
    if machine.exists(binary) {
        // Renaming works while it runs, on Windows too.
        machine
            .rename(binary, &replaced)
            .map_err(|error| format!("failed to move {binary} to {replaced}: {error}"))?;
    }
    machine
        .rename(moved, binary)
        .map_err(|error| format!("failed to move {moved} back to {binary}: {error}"))?;
    say(
        out,
        format_args!("Moved CLIProxyAPI's binary back to {binary}"),
    );
    let stopped: Vec<Proc> = running_binary(machine, platform, binary, &replaced)
        .into_iter()
        .filter(|process| Some(process.pid) != keep)
        .collect();
    for process in &stopped {
        machine.stop(process.pid, &process.name).map_err(|error| {
            format!(
                "failed to stop open-ferry (process {}): {error}",
                process.pid
            )
        })?;
        say(
            out,
            format_args!("Stopped open-ferry (process {})", process.pid),
        );
    }
    if machine.exists(&replaced) {
        if is_ours(machine, context, &replaced) {
            machine
                .remove(&replaced)
                .map_err(|error| format!("failed to remove {replaced}: {error}"))?;
        } else {
            say(
                out,
                format_args!(
                    "What was at {binary} isn't this open-ferry's binary (an update may have replaced it): it is kept at {replaced}."
                ),
            );
        }
    }
    Ok(stopped)
}

/// Starts `binary` with `process`'s command line, in its working directory.
fn start_like(
    machine: &mut dyn Machine,
    platform: Platform,
    binary: &str,
    process: &Proc,
    log: &str,
) -> Result<u32, String> {
    let launch = Launch {
        exe: binary.to_owned(),
        args: process.args.clone(),
        cwd: process
            .cwd
            .clone()
            .or_else(|| platform.parent(binary))
            .unwrap_or_default(),
        env: process.env.clone(),
        log: log.to_owned(),
    };
    machine
        .spawn(&launch)
        .map_err(|error| format!("failed to start {binary}: {error}"))
}

fn execute_drop_in(
    machine: &mut dyn Machine,
    base: &Base<'_>,
    yes: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    let context = base.context;
    let platform = context.platform;
    let found = base.found;
    let (Some(process), Some(binary), Starter::Launcher(launcher)) =
        (&found.process, &found.exe, &found.starter)
    else {
        return Err(
            "CLIProxyAPI's process or binary isn't known, so nothing was changed".to_owned(),
        );
    };
    let moved = format!("{binary}{MOVED_SUFFIX}");
    if machine.exists(&moved) {
        return Err(format!("{moved} already exists, so nothing was changed"));
    }
    let backup = back_up(
        machine,
        platform,
        base.assessment,
        base.config,
        base.backup_dir,
        out,
    )?;
    let mut record = new_record(
        machine,
        context,
        found,
        base.assessment,
        base.config,
        backup,
        Switch::DropIn {
            binary: binary.clone(),
            moved_to: moved.clone(),
            restarted: false,
            pid: process.pid,
        },
    );
    record::save(machine, platform, base.record_path, &record)
        .map_err(|error| format!("{error}\nNothing but the backup was made."))?;

    // The binaries swapped.
    if let Err(error) = machine.rename(binary, &moved) {
        finish(
            machine,
            platform,
            base.record_path,
            &mut record,
            Status::RolledBack,
            out,
        );
        return Err(format!(
            "Failed to move {binary} to {moved}: {error}. Nothing but the backup was made."
        ));
    }
    say(out, format_args!("Moved CLIProxyAPI's binary to {moved}"));
    if let Err(error) = machine.copy(&context.exe, binary) {
        let mut failures = Vec::new();
        if let Err(error) = machine.rename(&moved, binary) {
            failures.push(format!("failed to move {moved} back to {binary}: {error}"));
        }
        finish(
            machine,
            platform,
            base.record_path,
            &mut record,
            Status::RolledBack,
            out,
        );
        return Err(undone_message(
            &format!("failed to copy open-ferry to {binary}: {error}"),
            &failures,
        ));
    }
    say(out, format_args!("Copied open-ferry to {binary}"));

    // CLIProxyAPI stopped, and open-ferry started, with the yes.
    let question = format!(
        "Stop CLIProxyAPI (process {}) now and start open-ferry in its place, with the same command line? If not, {} yourself. [y/N]",
        process.pid,
        launcher.restart_hint()
    );
    let stop = yes || (machine.terminal() && machine.ask(&question));
    if !stop {
        finish(
            machine,
            platform,
            base.record_path,
            &mut record,
            Status::Switched,
            out,
        );
        say(
            out,
            format_args!(
                "open-ferry is in place of CLIProxyAPI's binary. CLIProxyAPI (process {}) runs until you {}; open-ferry starts in its place then. Check it then with `open-ferry check -config {}`.",
                process.pid,
                launcher.restart_hint(),
                platform.quote(base.config)
            ),
        );
        say(
            out,
            format_args!(
                "The switch's record is {}. To switch back: open-ferry {} -undo",
                base.record_path,
                super::NAME
            ),
        );
        return Ok(());
    }
    let listen = base.assessment.listen.as_ref();
    if let Err(error) = machine.stop(process.pid, &process.name) {
        let failures =
            match put_back_binary(machine, context, binary, &moved, Some(process.pid), out) {
                Ok(_) => Vec::new(),
                Err(error) => vec![error],
            };
        finish(
            machine,
            platform,
            base.record_path,
            &mut record,
            Status::RolledBack,
            out,
        );
        return Err(undone_message(
            &format!("CLIProxyAPI (process {}) didn't stop: {error}", process.pid),
            &failures,
        ));
    }
    say(
        out,
        format_args!("Stopped CLIProxyAPI (process {})", process.pid),
    );
    let restarted = wait_for_open_ferry(machine, listen, LAUNCHER_WAIT) == Ok(Verified::Yes);
    if restarted {
        let started = format!("{} started open-ferry", launcher_name(launcher));
        say(out, format_args!("{}", super::sentence(&started)));
    } else {
        let log = platform.join(base.backup_dir, "open-ferry.log");
        match start_like(machine, platform, binary, process, &log) {
            Ok(pid) => say(
                out,
                format_args!(
                    "Started open-ferry (process {pid}) with CLIProxyAPI's command line; its output goes to {log}"
                ),
            ),
            Err(error) => say(out, format_args!("{error}")),
        }
    }
    match wait_for_open_ferry(machine, listen, VERIFY_TIMEOUT) {
        Ok(verified) => {
            if let Switch::DropIn { restarted, .. } = &mut record.switch {
                *restarted = true;
            }
            finish(
                machine,
                platform,
                base.record_path,
                &mut record,
                Status::Switched,
                out,
            );
            switched(out, &verified, listen, base.record_path);
            Ok(())
        }
        Err(why) => {
            say(
                out,
                format_args!("open-ferry didn't answer: {why}. Undoing the switch."),
            );
            let mut failures = Vec::new();
            match put_back_binary(machine, context, binary, &moved, None, out) {
                Ok(_) => {
                    // The launcher may start it again; else it is started
                    // as it was.
                    if !matches!(
                        wait_for_any(machine, listen, LAUNCHER_WAIT),
                        Some(Answer::Other(_))
                    ) {
                        let log = platform.join(base.backup_dir, "cliproxyapi.log");
                        match start_like(machine, platform, binary, process, &log) {
                            Ok(pid) => say(
                                out,
                                format_args!(
                                    "Started CLIProxyAPI again (process {pid}); its output goes to {log}"
                                ),
                            ),
                            Err(error) => failures.push(error),
                        }
                    }
                }
                Err(error) => failures.push(error),
            }
            say_back(machine, listen, out);
            finish(
                machine,
                platform,
                base.record_path,
                &mut record,
                Status::RolledBack,
                out,
            );
            Err(undone_message(
                &format!("open-ferry didn't answer: {why}"),
                &failures,
            ))
        }
    }
}

/// What `-undo` would do, in words.
pub(crate) fn undo_steps(context: &Context, record: &Record, restore: bool) -> Vec<String> {
    let mut steps = Vec::new();
    match &record.switch {
        Switch::Service { theirs, ours } => {
            let ours = record::target_of(ours).map_or_else(
                || ours.clone(),
                |target| {
                    format!(
                        "{} ({})",
                        target.kind(),
                        target.location(context).unwrap_or_default()
                    )
                },
            );
            steps.push(format!(
                "Stop and remove the open-ferry service installed as {ours}."
            ));
            if restore {
                steps.push(restore_step(record));
            }
            steps.push(format!(
                "Turn {} back on as it was: {}.",
                record.cliproxyapi.started_by,
                listed(&start_commands(theirs))
            ));
        }
        Switch::DropIn {
            binary, moved_to, ..
        } => {
            steps.push(format!(
                "Move CLIProxyAPI's binary back from {moved_to} to {binary}, and remove open-ferry's copy."
            ));
            steps.push(format!(
                "Stop open-ferry where it runs from {binary}, and start CLIProxyAPI with the same command line, unless {} does.",
                record.cliproxyapi.started_by
            ));
            if restore {
                steps.push(restore_step(record));
            }
        }
    }
    if !restore {
        steps.push(
            "Leave the config and the auth directory as they are: with -restore, the backed-up ones are copied back.".to_owned(),
        );
    }
    steps
}

fn restore_step(record: &Record) -> String {
    match &record.backup {
        Some(backup) => {
            let files: Vec<&str> = backup
                .files
                .iter()
                .map(|copied| copied.from.as_str())
                .collect();
            format!("Copy back from {}: {}.", backup.dir, files.join(", "))
        }
        None => "There is no backup to copy back.".to_owned(),
    }
}

/// What `-restore` warns of.
pub(crate) fn restore_warning(record: &Record) -> String {
    format!(
        "-restore puts back the config, .env and credential files as they were at {}. A credential whose token was refreshed since has a new refresh token, and the old one, in the backup, may be refused: such a sign-in has to be made again. Files added since are left as they are.",
        record.created
    )
}

/// Switches back to CLIProxyAPI as `record` says, and marks it undone.
pub(crate) fn undo(
    machine: &mut dyn Machine,
    context: &Context,
    record_path: &str,
    record: &mut Record,
    restore_files: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    let platform = context.platform;
    let listen = record.cliproxyapi.listen.as_ref().map(|address| {
        let probe = super::assess::probe_address(&address.host);
        Listen {
            host: address.host.clone(),
            port: address.port,
            tls: address.tls,
            probe,
        }
    });
    let mut failures = Vec::new();
    // A service that didn't run before the switch isn't started again, so
    // nothing is waited for.
    let back = match &record.switch {
        Switch::Service { theirs, .. } => was_running(theirs),
        Switch::DropIn { .. } => true,
    };
    match record.switch.clone() {
        Switch::Service { theirs, ours } => {
            let target = record::target_of(&ours)
                .ok_or_else(|| format!("the record names an unknown service, {ours}"))?;
            if target.installed(machine, context)? {
                os_service::uninstall(machine, context, target, false, out)?;
            } else {
                say(
                    out,
                    format_args!("open-ferry isn't installed as {}; going on.", target.kind()),
                );
            }
            if restore_files
                && let Some(backup) = &record.backup
                && let Err(error) = restore(machine, platform, backup, out)
            {
                failures.push(error);
            }
            failures.extend(run_all(machine, &start_commands(&theirs), out));
        }
        Switch::DropIn {
            binary,
            moved_to,
            restarted,
            pid,
        } => {
            if !machine.exists(&moved_to) {
                return Err(format!(
                    "CLIProxyAPI's binary isn't at {moved_to}, so nothing was changed. Put it back at {binary} by hand."
                ));
            }
            // Unless it was restarted, CLIProxyAPI's process may still run.
            let keep = (!restarted).then_some(pid);
            let stopped = put_back_binary(machine, context, &binary, &moved_to, keep, out)?;
            if restore_files
                && let Some(backup) = &record.backup
                && let Err(error) = restore(machine, platform, backup, out)
            {
                failures.push(error);
            }
            if let Some(process) = stopped.first() {
                if !matches!(
                    wait_for_any(machine, listen.as_ref(), LAUNCHER_WAIT),
                    Some(Answer::Other(_))
                ) {
                    let log = record.backup.as_ref().map_or_else(
                        || platform.join(&context.cwd, "cliproxyapi.log"),
                        |backup| platform.join(&backup.dir, "cliproxyapi.log"),
                    );
                    match start_like(machine, platform, &binary, process, &log) {
                        Ok(pid) => say(
                            out,
                            format_args!(
                                "Started CLIProxyAPI (process {pid}) with the same command line; its output goes to {log}"
                            ),
                        ),
                        Err(error) => failures.push(error),
                    }
                }
            } else if let Some(pid) = keep
                && machine.running(pid)
            {
                say(
                    out,
                    format_args!(
                        "CLIProxyAPI (process {pid}) still runs, as it did before the switch: open-ferry never ran in its place."
                    ),
                );
            } else {
                say(
                    out,
                    format_args!(
                        "open-ferry wasn't running from {binary}. Start CLIProxyAPI as you do: {}.",
                        record.cliproxyapi.started_by
                    ),
                );
            }
        }
    }
    if back {
        say_back(machine, listen.as_ref(), out);
    }
    finish(machine, platform, record_path, record, Status::Undone, out);
    if failures.is_empty() {
        say(
            out,
            format_args!(
                "Switched back to CLIProxyAPI. The config and the credentials are {}; the backup is kept{}.",
                if restore_files {
                    "restored from the backup"
                } else {
                    "left as open-ferry had them"
                },
                record
                    .backup
                    .as_ref()
                    .map(|backup| format!(" in {}", backup.dir))
                    .unwrap_or_default()
            ),
        );
        Ok(())
    } else {
        Err(format!(
            "Switching back failed at: {}. See {GUIDE} to finish by hand.",
            failures.join("; ")
        ))
    }
}
