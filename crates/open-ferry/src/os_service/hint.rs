//! How to stop a service before replacing its installed file directly.
//! Supervised updates use `open-ferry update` without stopping the service.

use open_ferry_supervisor::endpoint::ServiceKind;

use super::{SERVICE_NAME, launchd, systemd};

/// The lines that tell a user to stop the service of `kind`, `again` (say,
/// "run `open-ferry update` again"), and start the service again, on this
/// system.
pub(crate) fn stop_and_retry(kind: ServiceKind, again: &str) -> String {
    let (stop, start) = commands(kind);
    let (whose, yourself) = match kind {
        ServiceKind::User => ("", "open-ferry -supervise"),
        ServiceKind::System if cfg!(windows) => (
            " in a terminal opened as administrator",
            "open-ferry -supervise -system",
        ),
        ServiceKind::System => (" as root", "open-ferry -supervise -system"),
    };
    format!(
        "Stop the service{whose}, {again}, and start the service again:\n  {stop}\n  {start}\n\
         If you started `{yourself}` yourself, stop that instead."
    )
}

/// The commands that stop and start the service of `kind` on this system.
fn commands(kind: ServiceKind) -> (String, String) {
    match kind {
        ServiceKind::User if cfg!(windows) => (
            format!("schtasks /end /tn {SERVICE_NAME}"),
            format!("schtasks /run /tn {SERVICE_NAME}"),
        ),
        ServiceKind::User if cfg!(target_os = "macos") => (
            format!("launchctl bootout gui/$(id -u)/{}", launchd::LABEL),
            format!(
                "launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/{}.plist",
                launchd::LABEL
            ),
        ),
        ServiceKind::User => (
            format!("systemctl --user stop {}", systemd::UNIT),
            format!("systemctl --user start {}", systemd::UNIT),
        ),
        ServiceKind::System if cfg!(windows) => (
            format!("sc.exe stop {SERVICE_NAME}"),
            format!("sc.exe start {SERVICE_NAME}"),
        ),
        ServiceKind::System if cfg!(target_os = "macos") => (
            format!("sudo launchctl bootout system/{}", launchd::LABEL),
            format!(
                "sudo launchctl bootstrap system /Library/LaunchDaemons/{}.plist",
                launchd::LABEL
            ),
        ),
        ServiceKind::System => (
            format!("sudo systemctl stop {}", systemd::UNIT),
            format!("sudo systemctl start {}", systemd::UNIT),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name() -> &'static str {
        if cfg!(windows) {
            SERVICE_NAME
        } else if cfg!(target_os = "macos") {
            launchd::LABEL
        } else {
            systemd::UNIT
        }
    }

    #[test]
    fn the_hint_names_this_systems_service_and_what_to_do_between() {
        let hint = stop_and_retry(ServiceKind::User, "run `open-ferry update` again");
        assert!(hint.contains("Stop the service, run `open-ferry update` again, and start"));
        let (stop, start) = commands(ServiceKind::User);
        assert!(hint.contains(&format!("\n  {stop}\n  {start}\n")));
        assert!(stop.contains(name()) && start.contains(name()));
        assert!(
            hint.ends_with("If you started `open-ferry -supervise` yourself, stop that instead.")
        );
    }

    #[test]
    fn the_system_hint_names_the_system_service_and_who_may_stop_it() {
        let hint = stop_and_retry(ServiceKind::System, "run the installer again");
        let (stop, start) = commands(ServiceKind::System);
        assert!(hint.contains(&format!("\n  {stop}\n  {start}\n")));
        assert!(stop.contains(name()) && start.contains(name()));
        if cfg!(windows) {
            assert!(hint.contains(
                "Stop the service in a terminal opened as administrator, run the installer again"
            ));
            assert!(stop.starts_with("sc.exe stop") && start.starts_with("sc.exe start"));
        } else {
            assert!(hint.contains("Stop the service as root, run the installer again"));
            assert!(stop.starts_with("sudo ") && start.starts_with("sudo "));
        }
        assert!(hint.ends_with(
            "If you started `open-ferry -supervise -system` yourself, stop that instead."
        ));
        assert_ne!(commands(ServiceKind::User), (stop, start));
    }
}
