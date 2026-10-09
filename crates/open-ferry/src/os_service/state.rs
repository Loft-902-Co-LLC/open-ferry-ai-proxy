//! What the service manager says of open-ferry's own service, for
//! `migrate`: after a service is stopped and removed, `migrate` waits for
//! the manager to say it has ended before CLIProxyAPI is started. A stop is
//! not always done when its command returns: `sc stop` only asks.
//!
//! Not upstream's: upstream has no `service`.

use super::{Account, Cmd, Context, SERVICE_NAME, System, Target, launchd, windows};

/// What `launchctl print` exits with for a job that isn't loaded.
const LAUNCHD_NOT_FOUND: i32 = 113;

/// How open-ferry's service stands with its manager.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ServiceState {
    /// The manager knows no such service.
    Gone,
    /// It is defined, and stopped.
    Stopped,
    /// It is loaded or running, in the manager's words.
    Running(String),
    /// The manager couldn't be asked, or said something this doesn't know.
    Unknown(String),
}

impl ServiceState {
    /// Whether the service has ended: it is gone or stopped.
    pub(crate) fn ended(&self) -> bool {
        matches!(self, ServiceState::Gone | ServiceState::Stopped)
    }

    /// The state, in words, for a message.
    pub(crate) fn describe(&self) -> String {
        match self {
            ServiceState::Gone => "gone".to_owned(),
            ServiceState::Stopped => "stopped".to_owned(),
            ServiceState::Running(what) => format!("still {what}"),
            ServiceState::Unknown(why) => format!("not known ({why})"),
        }
    }
}

/// Asks the service manager how `target`, open-ferry's service, stands.
pub(crate) fn service_state(
    system: &mut dyn System,
    context: &Context,
    target: Target,
) -> ServiceState {
    let cmd = match target {
        Target::SystemdUser | Target::SystemdSystem => {
            let mut cmd = Cmd::new("systemctl", &[]);
            if !target.system() {
                cmd = cmd.arg("--user");
            }
            cmd.arg("show")
                .arg(format!("{SERVICE_NAME}.service"))
                .arg("--property=LoadState,ActiveState")
        }
        Target::LaunchAgent | Target::LaunchDaemon => {
            let account = match Account::probe(system, context.platform, target) {
                Ok(account) => account,
                Err(error) => return ServiceState::Unknown(error),
            };
            launchd::status(&launchd::domain(target.system(), account.uid()))
        }
        Target::ScheduledTask => windows::query_task(),
        Target::WindowsService => windows::query_service(),
    };
    let output = match system.run(&cmd) {
        Ok(output) => output,
        Err(error) => return ServiceState::Unknown(format!("failed to run `{cmd}`: {error}")),
    };
    match target {
        Target::SystemdUser | Target::SystemdSystem => {
            if !output.success() {
                return ServiceState::Unknown(format!("`{cmd}` failed with {}", output.failure()));
            }
            let value = |name: &str| {
                output
                    .stdout
                    .lines()
                    .find_map(|line| line.trim().strip_prefix(name)?.strip_prefix('='))
                    .map(|value| value.trim().to_owned())
            };
            if value("LoadState").as_deref() == Some("not-found") {
                return ServiceState::Gone;
            }
            match value("ActiveState").as_deref() {
                Some("inactive" | "failed") => ServiceState::Stopped,
                Some(active) => ServiceState::Running(active.to_owned()),
                None => ServiceState::Unknown(format!("`{cmd}` didn't print ActiveState")),
            }
        }
        Target::LaunchAgent | Target::LaunchDaemon => {
            if output.success() {
                ServiceState::Running("loaded".to_owned())
            } else if output.code == Some(LAUNCHD_NOT_FOUND) {
                ServiceState::Gone
            } else {
                ServiceState::Unknown(format!("`{cmd}` failed with {}", output.failure()))
            }
        }
        // The task scheduler says only that the query failed. The list of
        // tasks says whether the task is there: when it can't be had
        // either, the state is not known, which is never "ended".
        Target::ScheduledTask => {
            if output.success() {
                return ServiceState::Running("registered".to_owned());
            }
            match system.run(&windows::list_tasks()) {
                Ok(listed) if listed.success() && !windows::task_listed(&listed.stdout) => {
                    ServiceState::Gone
                }
                Ok(listed) if listed.success() => ServiceState::Running("registered".to_owned()),
                _ => ServiceState::Unknown(format!("`{cmd}` failed with {}", output.failure())),
            }
        }
        Target::WindowsService => match output.code {
            Some(windows::SERVICE_DOES_NOT_EXIST) => ServiceState::Gone,
            Some(0) => match windows::service_state_number(&output.stdout) {
                Some(windows::SERVICE_STOPPED) => ServiceState::Stopped,
                Some(windows::SERVICE_RUNNING) => ServiceState::Running("running".to_owned()),
                Some(other) => ServiceState::Running(format!("in state {other}")),
                None => ServiceState::Unknown(format!("`{cmd}` didn't print a STATE")),
            },
            _ => ServiceState::Unknown(format!("`{cmd}` failed with {}", output.failure())),
        },
    }
}
