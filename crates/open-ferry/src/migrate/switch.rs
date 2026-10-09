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

/// What open-ferry's copy is renamed with when CLIProxyAPI's is put back.
const ASIDE: &str = ".open-ferry";

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
    /// For a drop-in, how open-ferry is put in place: `symlink` (Linux and
    /// macOS, with an install receipt: a link to the installed open-ferry,
    /// which updates itself) or `copy` (Windows, or no install receipt: a
    /// copy, which reports a release but doesn't install it).
    pub(crate) drop_in: Option<&'static str>,
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

/// The places of the switch's own files (the record, its temporary files,
/// its copy in the backup, and the backup) that are also the config, a
/// `.env` file or the auth directory, which `migrate` never writes over:
/// one message for each.
fn overlapping(
    machine: &dyn Machine,
    platform: Platform,
    record: &str,
    backup: &str,
    assessment: &Assessment,
) -> Vec<String> {
    let copy = platform.join(backup, record::FILE);
    let mut protected = Vec::new();
    if let Some(config) = &assessment.config {
        protected.push(("the config", config.as_str()));
    }
    for file in &assessment.env_files {
        protected.push(("the .env file", file.as_str()));
    }
    if let Some(auth) = &assessment.auth_dir {
        protected.push(("the auth directory", auth.as_str()));
    }
    let mut found = Vec::new();
    for (what, path) in protected {
        let real = machine.real_path(path).unwrap_or_else(|_| path.to_owned());
        let hit = [path, real.as_str()].into_iter().find_map(|spot| {
            let lower = spot.to_lowercase();
            // The temporary files are `<file>.<unique>.tmp`.
            let mine = [record, copy.as_str()].into_iter().find(|mine| {
                (lower.starts_with(&format!("{}.", mine.to_lowercase())) && lower.ends_with(".tmp"))
                    || discover::same_path(platform, spot, mine)
            });
            mine.or_else(|| platform.is_within(record, spot).then_some(record))
                .or_else(|| {
                    (discover::same_path(platform, spot, backup)
                        || platform.is_within(spot, backup))
                    .then_some(backup)
                })
        });
        if let Some(mine) = hit {
            found.push(format!(
                "{what} ({path}) is {mine} or inside it or holds it, and the switch writes there: move one of them. Nothing was changed."
            ));
        }
    }
    found
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
    commands_to_start(theirs, false)
}

/// The commands that put CLIProxyAPI's service back as it was, but for the
/// one that loads or starts it when `on`: it is on already.
fn commands_to_start(theirs: &Theirs, on: bool) -> Vec<Cmd> {
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
            if *was_loaded && !on {
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
            if *was_running && !on {
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
    let linked = kind == Kind::DropIn && link_to(machine, context).is_some();
    let mut plan = Plan {
        kind,
        steps: Vec::new(),
        blockers: Vec::new(),
        good_to_know: Vec::new(),
        // Placed by where the config and the auth directory really are, so
        // that a link can't put the backup inside the auth directory.
        backup_dir: assessment.config.as_deref().and_then(|config| {
            let real = |path: &str| machine.real_path(path).unwrap_or_else(|_| path.to_owned());
            backup_dir(
                platform,
                &real(config),
                assessment.auth_dir.as_deref().map(real).as_deref(),
                machine.now(),
            )
        }),
        record: record_path.clone(),
        command,
        theirs: theirs_of(&found.starter, found.process.is_some()),
        drop_in: (kind == Kind::DropIn).then_some(if linked { "symlink" } else { "copy" }),
    };
    match &record_path {
        Ok(path) => match record::load(machine, path) {
            Ok(Some(existing))
                if matches!(
                    existing.status,
                    Status::Switched | Status::Switching | Status::UndoneNotStarted
                ) =>
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
    if kind != Kind::Container
        && let (Ok(record), Some(backup)) = (plan.record.clone(), plan.backup_dir.clone())
    {
        let clash = overlapping(machine, platform, &record, &backup, assessment);
        plan.blockers.extend(clash);
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
    let link = link_to(machine, context);
    let source = link.as_deref().unwrap_or(&context.exe);
    if discover::same_path(platform, exe, &context.exe)
        || discover::same_path(platform, exe, source)
    {
        plan.blockers.push(format!(
            "{exe} is this open-ferry binary: run the open-ferry you installed, not one in CLIProxyAPI's place."
        ));
    }
    match &link {
        Some(target) => {
            plan.steps.push(format!(
                "Rename CLIProxyAPI's binary, {exe}, to {moved}, and make {exe} a symbolic link to the installed open-ferry, {target}, so that {} starts open-ferry.",
                launcher_name(launcher)
            ));
            plan.good_to_know.push(format!(
                "The link follows the installed open-ferry: `open-ferry update` replaces {target}, and {exe} runs the new version from the next start."
            ));
        }
        None => {
            plan.steps.push(format!(
                "Rename CLIProxyAPI's binary, {exe}, to {moved}, and copy open-ferry ({}) to {exe}, so that {} starts open-ferry.",
                context.exe,
                launcher_name(launcher)
            ));
            plan.good_to_know.push(format!(
                "The copy at {exe} reports a new release (`open-ferry update`) but doesn't install it: the updater replaces the installed open-ferry, not this copy. To update it, run `open-ferry {} -undo`, then `open-ferry {}` again."
                , super::NAME, super::NAME
            ));
            if platform != Platform::Windows {
                plan.good_to_know.push(
                    "There is no install receipt for this open-ferry (the install script writes one), so there is nothing for a link to follow: the drop-in is a copy. The copy says when a release is out but doesn't install it.".to_owned(),
                );
            }
        }
    }
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
/// symbolic links, `skip`, and at the top a `logs` directory. A symbolic
/// link where `to` is refuses the copy; one where a file goes is replaced
/// by the file. Nothing is written through a link.
fn copy_tree(
    machine: &mut dyn Machine,
    platform: Platform,
    from: &str,
    to: &str,
    skip: &str,
    top: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if let Some(target) = machine.link_target(to) {
        return Err(format!(
            "{to} is a symbolic link (to {target}), and is not written through: remove it or put a directory there, then try again"
        ));
    }
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
            EntryKind::File => copy_replacing_link(machine, &source, &target, out)?,
            EntryKind::Dir if !(top && entry.name == "logs") => {
                copy_tree(machine, platform, &source, &target, skip, false, out)?;
            }
            EntryKind::Dir | EntryKind::Link | EntryKind::Other => {}
        }
    }
    Ok(())
}

/// Copies the file `from` to `to`. A symbolic link at `to` is replaced by
/// the copy, not written through, and that is said.
fn copy_replacing_link(
    machine: &mut dyn Machine,
    from: &str,
    to: &str,
    out: &mut dyn Write,
) -> Result<(), String> {
    if let Some(target) = machine.link_target(to) {
        machine
            .remove(to)
            .map_err(|error| format!("failed to replace the symbolic link {to}: {error}"))?;
        say(
            out,
            format_args!("Replaced the symbolic link {to} (to {target}) with the file"),
        );
    }
    machine
        .copy(from, to)
        .map_err(|error| format!("failed to copy {from} to {to}: {error}"))
}

/// Copies the file `from` to `to`, for the backup.
fn copy_file(
    machine: &mut dyn Machine,
    from: &str,
    to: String,
    out: &mut dyn Write,
) -> Result<Copied, String> {
    copy_replacing_link(machine, from, &to, out)?;
    Ok(Copied {
        from: from.to_owned(),
        to,
        dir: false,
        real: None,
        parent: None,
    })
}

/// Notes where `copied.from` really is, and where its directory, for
/// `-restore` to check before it writes there.
fn located(machine: &dyn Machine, platform: Platform, mut copied: Copied) -> Copied {
    copied.real = machine.real_path(&copied.from).ok();
    copied.parent = platform
        .parent(&copied.from)
        .and_then(|parent| machine.real_path(&parent).ok());
    copied
}

/// A directory of the backup, made when it is first needed.
fn backup_subdir(
    machine: &mut dyn Machine,
    platform: Platform,
    dir: &str,
    name: &str,
) -> Result<String, String> {
    let sub = platform.join(dir, name);
    if !machine.exists(&sub) {
        machine
            .create_dir_all(&sub)
            .map_err(|error| format!("failed to create {sub}: {error}"))?;
    }
    Ok(sub)
}

/// Copies the config, its `.env` files and the auth directory into `dir`,
/// which is made private. Each kind of file has a directory of its own, so
/// that no name a config or a credential has can take another's place:
/// `config/<the config's name>`, `env/.env` and `env/working-dir.env`, and
/// `auth/`. The record's copy, `migration.json`, is beside them.
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
    let config_to = backup_subdir(machine, platform, dir, "config")?;
    let config_copy = copy_file(
        machine,
        config,
        platform.join(&config_to, &discover::file_name(platform, config)),
        out,
    )?;
    files.push(located(machine, platform, config_copy));
    let config_dir = platform.parent(config);
    for file in &assessment.env_files {
        let beside = platform
            .parent(file)
            .zip(config_dir.as_deref())
            .is_some_and(|(parent, config_dir)| discover::same_path(platform, &parent, config_dir));
        let name = if beside { ".env" } else { "working-dir.env" };
        let env_to = backup_subdir(machine, platform, dir, "env")?;
        let copied = copy_file(machine, file, platform.join(&env_to, name), out)?;
        files.push(located(machine, platform, copied));
    }
    if let Some(auth) = &assessment.auth_dir
        && machine.exists(auth)
    {
        // Where the auth directory really is, and where the backup is: a
        // link to the auth directory is followed once, here, and the backup,
        // which can be inside the auth directory, is skipped by its real
        // path so that the copy never copies itself.
        let real_auth = machine.real_path(auth).unwrap_or_else(|_| auth.clone());
        let real_dir = machine.real_path(dir).unwrap_or_else(|_| dir.to_owned());
        let to = platform.join(dir, "auth");
        copy_tree(machine, platform, &real_auth, &to, &real_dir, true, out)?;
        files.push(located(
            machine,
            platform,
            Copied {
                from: real_auth,
                to,
                dir: true,
                real: None,
                parent: None,
            },
        ));
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

/// Whether `now` is where `then` was, as the platform compares paths.
fn same_place(platform: Platform, now: Option<&str>, then: &str) -> bool {
    now.is_some_and(|now| discover::same_path(platform, now, then))
}

/// Why `copied` can't be put back: its directory, or it, leads somewhere
/// else than it did at the switch, so the write would change another place.
fn moved_since(machine: &dyn Machine, platform: Platform, copied: &Copied) -> Option<String> {
    let from = &copied.from;
    if let Some(then) = &copied.parent {
        let now = platform
            .parent(from)
            .and_then(|parent| machine.real_path(&parent).ok());
        if !same_place(platform, now.as_deref(), then) {
            return Some(format!(
                "{from} wasn't restored: its directory led to {then} at the switch and now leads to {}, so the write would change another place. Put the directory back, or copy the file by hand from {}",
                now.as_deref().unwrap_or("nowhere"),
                copied.to
            ));
        }
    }
    if let Some(then) = &copied.real
        && machine.exists(from)
    {
        let now = machine.real_path(from).ok();
        if !same_place(platform, now.as_deref(), then) {
            return Some(format!(
                "{from} wasn't restored: it led to {then} at the switch and now leads to {}, so the write would change another place. Put it back, or copy the file by hand from {}",
                now.as_deref().unwrap_or("nowhere"),
                copied.to
            ));
        }
    }
    None
}

/// Copies the backed-up files back. Each is tried, and the failures are
/// said and returned together. A file whose place has moved since the
/// switch is skipped, and said.
fn restore(
    machine: &mut dyn Machine,
    platform: Platform,
    backup: &Backup,
    out: &mut dyn Write,
) -> Result<(), String> {
    let mut failures = Vec::new();
    for copied in &backup.files {
        if let Some(why) = moved_since(machine, platform, copied) {
            say(out, format_args!("{why}"));
            failures.push(why);
            continue;
        }
        let done = if copied.dir {
            copy_tree(machine, platform, &copied.to, &copied.from, "", false, out)
        } else {
            copy_replacing_link(machine, &copied.to, &copied.from, out)
        };
        match done {
            Ok(()) => say(
                out,
                format_args!("Restored {} from {}", copied.from, copied.to),
            ),
            Err(error) => {
                say(out, format_args!("{error}"));
                failures.push(error);
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
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

/// Whether process `pid` runs and, when `started` is known, is the process
/// that started then: a process ID can be used again by another process.
fn same_process(machine: &mut dyn Machine, pid: u32, started: Option<u64>) -> bool {
    match started {
        Some(started) => machine.started(pid) == Some(started),
        None => machine.running(pid),
    }
}

/// Waits for process `pid`, which started at `started`, to exit.
fn wait_exit(machine: &mut dyn Machine, pid: u32, started: Option<u64>, timeout: Duration) -> bool {
    let tries = timeout.as_millis() / POLL.as_millis();
    for attempt in 0..=tries {
        if !same_process(machine, pid, started) {
            return true;
        }
        if attempt < tries {
            machine.sleep(POLL);
        }
    }
    false
}

/// What `migrate` waits to have ended.
enum Ending<'a> {
    /// open-ferry's service, as its manager says.
    Service(Target),
    /// These processes of open-ferry.
    Processes(&'a [Proc]),
}

/// Waits for open-ferry to have ended, so that CLIProxyAPI isn't started
/// beside it. The service or the processes themselves are what is waited
/// for: the manager's state, or the processes' identity. That nothing
/// answers on the address is further evidence, not the proof, and is only
/// asked when the address can be.
fn wait_ended(
    machine: &mut dyn Machine,
    context: &Context,
    ending: &Ending<'_>,
    listen: Option<&Listen>,
) -> Result<(), String> {
    let tries = (EXIT_WAIT.as_millis() / POLL.as_millis()).max(1);
    let mut last = String::new();
    for attempt in 0..tries {
        if attempt > 0 {
            machine.sleep(POLL);
        }
        let mut why = match ending {
            Ending::Service(target) => {
                let state = os_service::service_state(machine, context, *target);
                (!state.ended()).then(|| format!("its service is {}", state.describe()))
            }
            Ending::Processes(processes) => processes
                .iter()
                .find(|process| same_process(machine, process.pid, process.started))
                .map(|process| format!("process {} still runs", process.pid)),
        };
        if why.is_none()
            && let Some(listen) = listen
            && let Some(ip) = listen.probe
            && machine.probe(ip, listen.port, listen.tls) == Answer::OpenFerry
        {
            why = Some(format!("open-ferry still answers on {}", address(listen)));
        }
        match why {
            None => return Ok(()),
            Some(why) => last = why,
        }
    }
    Err(format!(
        "open-ferry hasn't ended {} seconds after it was stopped ({last}), so CLIProxyAPI is not started beside it. The record stays open: run `open-ferry {} -undo` again once open-ferry has stopped",
        EXIT_WAIT.as_secs(),
        super::NAME
    ))
}

/// Whether CLIProxyAPI's service is on already, as its manager says.
fn is_on(machine: &mut dyn Machine, theirs: &Theirs) -> bool {
    let cmd = match theirs {
        Theirs::Launchd {
            label,
            domain,
            was_loaded: true,
            ..
        } => Cmd::new("launchctl", &["print"]).arg(format!("{domain}/{label}")),
        Theirs::WindowsService {
            name,
            was_running: true,
            ..
        } => Cmd::new("sc.exe", &["query", name]),
        _ => return false,
    };
    machine.run(&cmd).is_ok_and(|output| {
        output.success()
            && match theirs {
                Theirs::WindowsService { .. } => output.stdout.contains("RUNNING"),
                _ => true,
            }
    })
}

/// Turns CLIProxyAPI's service back on as it was. The manager is asked
/// first: a job already loaded or a service already running counts as done,
/// so that doing this again after it was cut short works. The failures are
/// returned.
fn start_theirs(machine: &mut dyn Machine, theirs: &Theirs, out: &mut dyn Write) -> Vec<String> {
    let on = is_on(machine, theirs);
    if on {
        say(
            out,
            format_args!("CLIProxyAPI's service is on already; going on."),
        );
    }
    run_all(machine, &commands_to_start(theirs, on), out)
}

/// Turns CLIProxyAPI's service back on once open-ferry's has ended; the
/// failures are returned.
fn turn_theirs_on(
    machine: &mut dyn Machine,
    context: &Context,
    target: Target,
    theirs: &Theirs,
    listen: Option<&Listen>,
    out: &mut dyn Write,
) -> Vec<String> {
    if let Err(error) = wait_ended(machine, context, &Ending::Service(target), listen) {
        say(out, format_args!("{error}"));
        return vec![error];
    }
    start_theirs(machine, theirs, out)
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
    let clash = overlapping(
        machine,
        context.platform,
        &record_path,
        &backup_dir,
        assessment,
    );
    if !clash.is_empty() {
        return Err(clash.join("\n"));
    }
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
        let failures = start_theirs(machine, &theirs, out);
        settle(
            machine,
            platform,
            base.record_path,
            &mut record,
            Status::RolledBack,
            &failures,
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
        // CLIProxyAPI's service isn't turned on beside one that stays.
        if failures.is_empty() {
            failures.extend(turn_theirs_on(
                machine, context, target, &theirs, listen, out,
            ));
        }
        if failures.is_empty() && was_running(&theirs) {
            say_back(machine, listen, out);
        }
        settle(
            machine,
            platform,
            base.record_path,
            &mut record,
            Status::RolledBack,
            &failures,
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
            if failures.is_empty() {
                failures.extend(turn_theirs_on(
                    machine, context, target, &theirs, listen, out,
                ));
            }
            if failures.is_empty() && was_running(&theirs) {
                say_back(machine, listen, out);
            }
            settle(
                machine,
                platform,
                base.record_path,
                &mut record,
                Status::RolledBack,
                &failures,
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
            "The switch is undone as far as it could be: {why}. Undoing it failed at: {}. The record stays open: run `open-ferry {} -undo` to finish what is left, or see {GUIDE} to finish by hand.",
            failures.join("; "),
            super::NAME
        )
    }
}

/// Saves `record` once a switch that failed is undone: as `status` when
/// every step of undoing it worked, else as it was, still open, so that
/// `-undo` can finish what is left.
fn settle(
    machine: &mut dyn Machine,
    platform: Platform,
    path: &str,
    record: &mut Record,
    status: Status,
    failures: &[String],
    out: &mut dyn Write,
) {
    let status = if failures.is_empty() {
        status
    } else {
        Status::Switching
    };
    finish(machine, platform, path, record, status, out);
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
        && same_process(machine, process.pid, process.started)
    {
        machine
            .stop(process.pid, &process.name, process.started)
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
    if !wait_exit(machine, process.pid, process.started, EXIT_WAIT) {
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

/// Where a drop-in's symbolic link points, on Linux and macOS: the binary
/// the install receipt names, which `open-ferry update` replaces. `None`
/// where the drop-in is a copy: on Windows, and without a receipt, which
/// leaves nothing for a link to follow (and a link to this exe's own path
/// can go stale, such as a Homebrew Cellar path after `brew upgrade`).
fn link_to(machine: &mut dyn Machine, context: &Context) -> Option<String> {
    if context.platform == Platform::Windows {
        return None;
    }
    let binary = machine.installed_binary()?;
    machine.exists(&binary).then_some(binary)
}

/// Whether the process `process`, which runs the link target, was started
/// through `binary`: the first word of its command line, taken from its
/// working directory when it is relative, is `binary`. A bare name is not:
/// it could be any file of that name on the search path.
///
/// UNVERIFIED on macOS: whether `sysinfo` gives the command line there as
/// the process was started.
fn started_through(
    machine: &mut dyn Machine,
    platform: Platform,
    process: &Proc,
    binary: &str,
) -> bool {
    let Some(first) = machine.argv0(process.pid) else {
        return false;
    };
    if !first.chars().any(|c| platform.is_separator(c)) {
        return false;
    }
    let path = if platform.is_absolute(&first) {
        first
    } else {
        match &process.cwd {
            Some(cwd) => platform.join(cwd, &first),
            None => return false,
        }
    };
    discover::same_path(platform, &path, binary)
}

/// The processes of the drop-in: those that run open-ferry from `binary` or
/// from `others` (its copy, set aside), and, where `binary` is a link to
/// `link`, those that run `link` and were started through `binary`. An
/// open-ferry that wasn't started through `binary` is not the drop-in, and
/// is never taken for it.
fn running_binary(
    machine: &mut dyn Machine,
    platform: Platform,
    binary: &str,
    others: &[&str],
    link: Option<&str>,
    theirs_at_binary: bool,
) -> Vec<Proc> {
    // A process started through the link is named for the link, or, on
    // macOS, may be named for what it leads to.
    let mut names = vec![discover::file_name(platform, binary)];
    if let Some(link) = link {
        names.push(discover::file_name(platform, link));
    }
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let real_link = link.map(|link| machine.real_path(link).unwrap_or_else(|_| link.to_owned()));
    let mut found = Vec::new();
    for process in machine.processes(&names).unwrap_or_default() {
        let Some(exe) = process.exe.clone() else {
            continue;
        };
        // Linux names a replaced binary `<path> (deleted)`.
        let gone = exe.ends_with(" (deleted)");
        let exe = exe.strip_suffix(" (deleted)").unwrap_or(&exe).to_owned();
        // With CLIProxyAPI's binary at `binary`, a process running a file
        // that is still there is CLIProxyAPI's: open-ferry's copy has been
        // moved (and runs from where it went) or removed (and is deleted).
        let copy = (discover::same_path(platform, &exe, binary) && (gone || !theirs_at_binary))
            || others
                .iter()
                .any(|other| discover::same_path(platform, &exe, other));
        let through = !copy
            && [link, real_link.as_deref()]
                .into_iter()
                .flatten()
                .any(|target| discover::same_path(platform, &exe, target))
            && started_through(machine, platform, &process, binary);
        if copy || through {
            found.push(process);
        }
    }
    found
}

/// Whether the file at `path` is this open-ferry's binary.
fn is_ours(machine: &dyn Machine, context: &Context, path: &str) -> bool {
    match (machine.read(path), machine.read(&context.exe)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// A process, by its ID and when it started.
type Running = (u32, Option<u64>);

/// Whether `process` is the one `keep` names.
fn is_kept(process: &Proc, keep: Option<Running>) -> bool {
    keep.is_some_and(|(pid, started)| {
        process.pid == pid && (started.is_none() || process.started == started)
    })
}

/// The SHA-256 of the file at `path`, in hex, if it can be read.
fn digest(machine: &dyn Machine, path: &str) -> Option<String> {
    machine
        .read(path)
        .ok()
        .map(|data| open_ferry_update::release::sha256_hex(&data))
}

/// Whether the file at `path` has the digest `want`, recorded at the switch.
fn has_digest(machine: &dyn Machine, path: &str, want: &str) -> bool {
    !want.is_empty() && digest(machine, path).is_some_and(|got| got.eq_ignore_ascii_case(want))
}

/// What is at `path`, in words, for a message.
fn found_at(machine: &dyn Machine, path: &str) -> String {
    if let Some(target) = machine.link_target(path) {
        format!("a symbolic link to {target}")
    } else if machine.exists(path) {
        match digest(machine, path) {
            Some(got) => format!("a file whose SHA-256 is {got}"),
            None => "a file that can't be read".to_owned(),
        }
    } else {
        "nothing".to_owned()
    }
}

/// A drop-in's binary and where it went.
struct Placed<'a> {
    /// Where open-ferry was put, and CLIProxyAPI's binary was.
    binary: &'a str,
    /// Where CLIProxyAPI's binary was moved.
    moved: &'a str,
    /// The SHA-256 of CLIProxyAPI's binary at the switch: only a file with
    /// it is taken for CLIProxyAPI's.
    sha256: &'a str,
    /// Where `binary` is a symbolic link to.
    link: Option<&'a str>,
}

/// Puts CLIProxyAPI's binary back at `binary` from `moved`, and stops what
/// runs open-ferry from there (or from `link`, where a symbolic link to
/// open-ferry led), but for the process `keep`, CLIProxyAPI's own; gives what
/// it stopped. Only a file with the digest recorded at the switch is taken
/// for CLIProxyAPI's: anything else, such as an older open-ferry, is left
/// as it is, and said. Each step looks at what is there, so that doing it
/// again after one failed finishes it.
fn put_back_binary(
    machine: &mut dyn Machine,
    context: &Context,
    placed: &Placed<'_>,
    keep: Option<Running>,
    out: &mut dyn Write,
) -> Result<Vec<Proc>, String> {
    let platform = context.platform;
    let Placed {
        binary,
        moved,
        sha256,
        link,
    } = *placed;
    let plain = format!("{binary}{ASIDE}");
    let replaced = aside(machine, binary, ASIDE);
    let manual = |machine: &dyn Machine, what: String| {
        format!(
            "{what}. CLIProxyAPI's binary (SHA-256 {sha256}) wasn't put back, and the record stays open: put it at {binary} by hand (from the release you installed, or your package manager), then run `open-ferry {} -undo` again.",
            super::NAME
        ) + &format!(" At {binary} there is {}.", found_at(machine, binary))
    };
    if machine.exists(moved) {
        if !has_digest(machine, moved, sha256) {
            return Err(manual(
                machine,
                format!(
                    "{moved} isn't CLIProxyAPI's binary: its SHA-256 isn't the one recorded at the switch"
                ),
            ));
        }
        if machine.link_target(binary).is_some() {
            // A link takes nothing with it: it is only removed.
            machine
                .remove(binary)
                .map_err(|error| format!("failed to remove the link {binary}: {error}"))?;
        } else if machine.exists(binary) {
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
    } else if machine.exists(binary)
        && machine.link_target(binary).is_none()
        && has_digest(machine, binary, sha256)
    {
        say(
            out,
            format_args!(
                "{moved} isn't there: taking {binary} to be CLIProxyAPI's binary, as put back before; going on."
            ),
        );
    } else {
        return Err(manual(
            machine,
            format!("CLIProxyAPI's binary isn't at {moved}, and what is at {binary} isn't it"),
        ));
    }
    let places = [plain.as_str(), replaced.as_str()];
    let theirs_at_binary =
        machine.link_target(binary).is_none() && has_digest(machine, binary, sha256);
    let stopped: Vec<Proc> =
        running_binary(machine, platform, binary, &places, link, theirs_at_binary)
            .into_iter()
            .filter(|process| !is_kept(process, keep))
            .collect();
    for process in &stopped {
        machine
            .stop(process.pid, &process.name, process.started)
            .map_err(|error| {
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
    let mut leftovers = vec![plain.clone()];
    if replaced != plain {
        leftovers.push(replaced);
    }
    for left in leftovers {
        if !machine.exists(&left) {
            continue;
        }
        if is_ours(machine, context, &left) {
            machine
                .remove(&left)
                .map_err(|error| format!("failed to remove {left}: {error}"))?;
        } else {
            say(
                out,
                format_args!(
                    "What was at {binary} isn't this open-ferry's binary (an update may have replaced it): it is kept at {left}."
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
    let link = link_to(machine, context);
    let sha256 = digest(machine, binary).ok_or_else(|| {
        format!(
            "CLIProxyAPI's binary, {binary}, can't be read to record its digest, so nothing was changed"
        )
    })?;
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
            started: process.started,
            link: link.clone(),
            sha256: sha256.clone(),
        },
    );
    let placed = Placed {
        binary,
        moved: &moved,
        sha256: &sha256,
        link: link.as_deref(),
    };
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
    let put = match &link {
        Some(target) => machine
            .symlink(target, binary)
            .map(|()| format!("Linked {binary} to {target}, the installed open-ferry")),
        None => machine
            .copy(&context.exe, binary)
            .map(|()| format!("Copied open-ferry to {binary}")),
    };
    match put {
        Ok(done) => say(out, format_args!("{done}")),
        Err(error) => {
            let mut failures = Vec::new();
            if let Err(error) = machine.rename(&moved, binary) {
                failures.push(format!("failed to move {moved} back to {binary}: {error}"));
            }
            settle(
                machine,
                platform,
                base.record_path,
                &mut record,
                Status::RolledBack,
                &failures,
                out,
            );
            return Err(undone_message(
                &format!("failed to put open-ferry at {binary}: {error}"),
                &failures,
            ));
        }
    }

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
    if let Err(error) = machine.stop(process.pid, &process.name, process.started) {
        let failures = match put_back_binary(
            machine,
            context,
            &placed,
            Some((process.pid, process.started)),
            out,
        ) {
            Ok(_) => Vec::new(),
            Err(error) => vec![error],
        };
        settle(
            machine,
            platform,
            base.record_path,
            &mut record,
            Status::RolledBack,
            &failures,
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
            match put_back_binary(machine, context, &placed, None, out) {
                Ok(stopped) => {
                    match wait_ended(machine, context, &Ending::Processes(&stopped), listen) {
                        Err(error) => {
                            say(out, format_args!("{error}"));
                            failures.push(error);
                        }
                        // The launcher may start it again; else it is started
                        // as it was.
                        Ok(()) => {
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
                    }
                }
                Err(error) => failures.push(error),
            }
            if failures.is_empty() {
                say_back(machine, listen, out);
            }
            settle(
                machine,
                platform,
                base.record_path,
                &mut record,
                Status::RolledBack,
                &failures,
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
                steps.push(restore_guard());
            }
            steps.push(format!(
                "Turn {} back on as it was, once open-ferry has stopped: {}.",
                record.cliproxyapi.started_by,
                listed(&start_commands(theirs))
            ));
        }
        Switch::DropIn {
            binary,
            moved_to,
            link,
            ..
        } => {
            steps.push(if link.is_some() {
                format!(
                    "Remove the symbolic link {binary}, and move CLIProxyAPI's binary back from {moved_to} to {binary}."
                )
            } else {
                format!(
                    "Move CLIProxyAPI's binary back from {moved_to} to {binary}, and remove open-ferry's copy."
                )
            });
            steps.push(format!(
                "Stop open-ferry where it runs from {binary}, and start CLIProxyAPI with the same command line, unless {} does. Only a file with the SHA-256 recorded at the switch is put back as CLIProxyAPI's. If CLIProxyAPI can't be started or found running, the record stays open (undone, but CLIProxyAPI not started): start it as you do, then run -undo again.",
                record.cliproxyapi.started_by
            ));
            if restore {
                steps.push(restore_step(record));
                steps.push(restore_guard());
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

/// The plan's line for stopping a proxy that uses the files.
fn restore_guard() -> String {
    "Before the copy, if CLIProxyAPI or open-ferry runs on those files, ask whether to stop it (-yes is the yes); nothing is stopped without it, and nothing is copied while it runs. A file whose directory now leads somewhere else than at the switch is skipped. If the copy fails, CLIProxyAPI isn't started and the record stays open: run -undo -restore again.".to_owned()
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
        "-restore puts back the config, .env and credential files as they were at {}. The credentials come back as they were at the switch, so a token refreshed since then is replaced by the older one. A credential whose token was refreshed since has a new refresh token, and the old one, in the backup, may be refused: such a sign-in has to be made again. Files added since are left as they are.",
        record.created
    )
}

/// The processes that run CLIProxyAPI's binary, at one of `paths` or kept
/// as the process `keep`.
fn running_theirs(
    machine: &mut dyn Machine,
    platform: Platform,
    paths: &[String],
    keep: Option<Running>,
) -> Vec<Proc> {
    let names: Vec<String> = paths
        .iter()
        .map(|path| discover::file_name(platform, path))
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    machine
        .processes(&names)
        .unwrap_or_default()
        .into_iter()
        .filter(|process| {
            is_kept(process, keep)
                || process.exe.as_deref().is_some_and(|exe| {
                    let exe = exe.strip_suffix(" (deleted)").unwrap_or(exe);
                    paths
                        .iter()
                        .any(|path| discover::same_path(platform, exe, path))
                })
        })
        .collect()
}

/// What `-restore` found using the files, and stopped.
struct Restored {
    /// CLIProxyAPI's processes stopped for the copy.
    stopped: Vec<Proc>,
    /// Why the copy didn't finish, if it didn't.
    failure: Option<String>,
}

/// Copies the backed-up files back, but not under a proxy that runs: a
/// CLIProxyAPI still running (or restarted) is stopped first, with the
/// person's yes (`yes` is `-yes`), and if something else still answers on
/// the address, nothing is copied.
fn restore_safely(
    machine: &mut dyn Machine,
    context: &Context,
    record: &Record,
    listen: Option<&Listen>,
    yes: bool,
    out: &mut dyn Write,
) -> Restored {
    let platform = context.platform;
    let fail = |failure: String| Restored {
        stopped: Vec::new(),
        failure: Some(failure),
    };
    let Some(backup) = &record.backup else {
        return Restored {
            stopped: Vec::new(),
            failure: None,
        };
    };
    let mut paths = Vec::new();
    let mut keep = None;
    if let Some(exe) = &record.cliproxyapi.exe {
        paths.push(exe.clone());
    }
    if let Switch::DropIn {
        binary,
        moved_to,
        restarted,
        pid,
        started,
        ..
    } = &record.switch
    {
        paths.push(binary.clone());
        paths.push(moved_to.clone());
        keep = (!restarted).then_some((*pid, *started));
    }
    let users = running_theirs(machine, platform, &paths, keep);
    if users.is_empty() {
        if let Some(listen) = listen
            && let Some(ip) = listen.probe
        {
            let answer = machine.probe(ip, listen.port, listen.tls);
            if !matches!(answer, Answer::Nothing(_)) {
                return fail(format!(
                    "something still answers on {} ({}), so the files may be in use: nothing was restored. Stop it, then run `open-ferry {} -undo -restore` again",
                    address(listen),
                    match answer {
                        Answer::Other(what) => what,
                        _ => "open-ferry".to_owned(),
                    },
                    super::NAME
                ));
            }
        }
    } else {
        let list: Vec<String> = users
            .iter()
            .map(|process| process.pid.to_string())
            .collect();
        let list = list.join(", ");
        let question = format!(
            "CLIProxyAPI runs (process {list}) on the files -restore replaces. Stop it now? It is started again once they are back. [y/N]"
        );
        if !(yes || (machine.terminal() && machine.ask(&question))) {
            return fail(format!(
                "CLIProxyAPI runs (process {list}), and -restore doesn't write under a running proxy: nothing was restored. Stop it yourself, or run `open-ferry {} -undo -restore -yes` to have it stopped",
                super::NAME
            ));
        }
        let mut stopped = Vec::new();
        for process in users {
            match machine.stop(process.pid, &process.name, process.started) {
                Ok(()) => {
                    say(
                        out,
                        format_args!("Stopped CLIProxyAPI (process {})", process.pid),
                    );
                    stopped.push(process);
                }
                Err(error) => {
                    return Restored {
                        stopped,
                        failure: Some(format!(
                            "failed to stop CLIProxyAPI (process {}): {error}. Nothing was restored",
                            process.pid
                        )),
                    };
                }
            }
        }
        if let Err(error) = wait_ended(machine, context, &Ending::Processes(&stopped), None) {
            return Restored {
                stopped,
                failure: Some(error),
            };
        }
        let failure = restore(machine, platform, backup, out).err();
        return Restored { stopped, failure };
    }
    Restored {
        stopped: Vec::new(),
        failure: restore(machine, platform, backup, out).err(),
    }
}

/// Whether CLIProxyAPI is found running: its binary runs as a process, or
/// something other than open-ferry answers on its address.
fn theirs_runs(
    machine: &mut dyn Machine,
    platform: Platform,
    paths: &[String],
    listen: Option<&Listen>,
) -> bool {
    if !running_theirs(machine, platform, paths, None).is_empty() {
        return true;
    }
    listen
        .and_then(|listen| {
            listen
                .probe
                .map(|ip| machine.probe(ip, listen.port, listen.tls))
        })
        .is_some_and(|answer| matches!(answer, Answer::Other(_)))
}

/// Switches back to CLIProxyAPI as `record` says. The record is marked
/// undone only when every step worked; else it stays as it was, and `-undo`
/// can be run again, each step looking at what is there. `yes` is `-yes`.
pub(crate) fn undo(
    machine: &mut dyn Machine,
    context: &Context,
    record_path: &str,
    record: &mut Record,
    restore_files: bool,
    yes: bool,
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
    // Whether CLIProxyAPI's files are back but it isn't running, and
    // `migrate` can't start it.
    let mut not_started = false;
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
            // CLIProxyAPI's service isn't turned on while open-ferry's
            // service or process is there.
            match wait_ended(machine, context, &Ending::Service(target), listen.as_ref()) {
                Err(error) => {
                    say(out, format_args!("{error}"));
                    failures.push(error);
                }
                Ok(()) => {
                    let mut copied = true;
                    if restore_files {
                        let done =
                            restore_safely(machine, context, record, listen.as_ref(), yes, out);
                        if let Some(error) = done.failure {
                            say(out, format_args!("{error}"));
                            failures.push(error);
                            copied = false;
                        }
                        if !done.stopped.is_empty() && !was_running(&theirs) {
                            say(
                                out,
                                format_args!(
                                    "CLIProxyAPI was stopped for the copy, and its service wasn't running before the switch: start it as you do."
                                ),
                            );
                        }
                    }
                    if copied {
                        failures.extend(start_theirs(machine, &theirs, out));
                    } else {
                        say(
                            out,
                            format_args!(
                                "CLIProxyAPI's service is not turned on: the copy didn't finish."
                            ),
                        );
                    }
                }
            }
        }
        Switch::DropIn {
            binary,
            moved_to,
            restarted,
            pid,
            started,
            link,
            sha256,
        } => {
            // Unless it was restarted, CLIProxyAPI's process may still run.
            let keep = (!restarted).then_some((pid, started));
            let placed = Placed {
                binary: &binary,
                moved: &moved_to,
                sha256: &sha256,
                link: link.as_deref(),
            };
            let mut stopped = put_back_binary(machine, context, &placed, keep, out)?;
            let mut copied = true;
            if restore_files {
                let done = restore_safely(machine, context, record, listen.as_ref(), yes, out);
                if let Some(error) = done.failure {
                    say(out, format_args!("{error}"));
                    failures.push(error);
                    copied = false;
                }
                // What CLIProxyAPI is started like: open-ferry's command
                // line, else its own, as it was stopped.
                stopped.extend(done.stopped);
            }
            if !copied {
                say(
                    out,
                    format_args!(
                        "CLIProxyAPI is not started: the copy didn't finish. Start it as you do once it has: {}.",
                        record.cliproxyapi.started_by
                    ),
                );
            } else if let Some(process) = stopped.first() {
                match wait_ended(
                    machine,
                    context,
                    &Ending::Processes(&stopped),
                    listen.as_ref(),
                ) {
                    Err(error) => {
                        say(out, format_args!("{error}"));
                        failures.push(error);
                    }
                    Ok(()) => {
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
                    }
                }
            } else if let Some((pid, started)) = keep
                && same_process(machine, pid, started)
            {
                say(
                    out,
                    format_args!(
                        "CLIProxyAPI (process {pid}) still runs, as it did before the switch: open-ferry never ran in its place."
                    ),
                );
            } else if theirs_runs(
                machine,
                platform,
                &[binary.clone(), moved_to.clone()],
                listen.as_ref(),
            ) {
                say(out, format_args!("CLIProxyAPI is running."));
            } else {
                not_started = true;
                say(
                    out,
                    format_args!(
                        "open-ferry wasn't running from {binary}, and CLIProxyAPI isn't running. Start CLIProxyAPI as you do: {}.",
                        record.cliproxyapi.started_by
                    ),
                );
            }
        }
    }
    if failures.is_empty() && not_started {
        finish(
            machine,
            platform,
            record_path,
            record,
            Status::UndoneNotStarted,
            out,
        );
        return Err(format!(
            "CLIProxyAPI's files are back, but CLIProxyAPI isn't running, and `migrate` has no command line to start it with. Start CLIProxyAPI as you do ({}), then run `open-ferry {} -undo` again: the record stays open until CLIProxyAPI is found running.",
            record.cliproxyapi.started_by,
            super::NAME
        ));
    }
    if failures.is_empty() {
        if back {
            say_back(machine, listen.as_ref(), out);
        }
        finish(machine, platform, record_path, record, Status::Undone, out);
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
            "Switching back failed at: {}. The record stays open: run `open-ferry {} -undo` again to finish what is left, or see {GUIDE} to finish by hand.",
            failures.join("; "),
            super::NAME
        ))
    }
}
