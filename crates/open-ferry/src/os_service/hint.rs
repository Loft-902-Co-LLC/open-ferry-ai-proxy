//! What to tell a user whose update or install meets a running supervisor:
//! updating while it runs comes in a later version, so stop the background
//! service, try again, and start the service again.

use super::{SERVICE_NAME, launchd, systemd};

/// The lines that tell a user to stop the user service, `again` (say, "run
/// `open-ferry update` again"), and start the service again, on this system.
pub(crate) fn stop_and_retry(again: &str) -> String {
    let (stop, start) = commands();
    format!(
        "Updating open-ferry while its supervisor runs comes in a later version. \
         Stop the service, {again}, and start the service again:\n  {stop}\n  {start}\n\
         If you started `open-ferry -supervise` yourself, stop that instead."
    )
}

/// The commands that stop and start the user service on this system.
fn commands() -> (String, String) {
    if cfg!(windows) {
        (
            format!("schtasks /end /tn {SERVICE_NAME}"),
            format!("schtasks /run /tn {SERVICE_NAME}"),
        )
    } else if cfg!(target_os = "macos") {
        (
            format!("launchctl bootout gui/$(id -u)/{}", launchd::LABEL),
            format!(
                "launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/{}.plist",
                launchd::LABEL
            ),
        )
    } else {
        (
            format!("systemctl --user stop {}", systemd::UNIT),
            format!("systemctl --user start {}", systemd::UNIT),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hint_names_this_systems_service_and_what_to_do_between() {
        let hint = stop_and_retry("run `open-ferry update` again");
        assert!(hint.contains("Stop the service, run `open-ferry update` again, and start"));
        let (stop, start) = commands();
        assert!(hint.contains(&format!("\n  {stop}\n  {start}\n")));
        let name = if cfg!(windows) {
            SERVICE_NAME
        } else if cfg!(target_os = "macos") {
            launchd::LABEL
        } else {
            systemd::UNIT
        };
        assert!(stop.contains(name) && start.contains(name));
    }
}
