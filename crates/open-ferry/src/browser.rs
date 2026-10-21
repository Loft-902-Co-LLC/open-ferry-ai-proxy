// Ported from CLIProxyAPI internal/browser/browser.go and
// internal/tui/browser.go (openBrowser) (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Opening a URL in the user's browser, for the OAuth logins and the TUI's
//! OAuth tab. For the TUI it is what upstream's `openBrowser` runs.
//!
//! Deviations from upstream:
//! - For the logins, it runs the platform's own opener (`rundll32 url.dll,FileProtocolHandler`
//!   on Windows, `open` on macOS, `xdg-open` elsewhere) and nothing else.
//!   Upstream tries a list of fallbacks, and checks a browser is there by
//!   opening `about:blank`.

use std::io;
use std::process::{Command, Stdio};

/// Opens `url` in the default browser, without waiting for it.
pub fn open(url: &str) -> io::Result<()> {
    open_with(url, crate::scope::runtime_probe().as_ref(), spawn)
}

/// Why the caller must open the URL itself, if browser execution is unsafe.
pub(crate) fn refusal(probe: &dyn crate::scope::Probe) -> Option<&'static str> {
    if !probe.in_container() && !matches!(probe.privileged(), Ok(false)) {
        Some(
            "The browser isn't opened while running with administrator or root rights. Open the URL yourself.",
        )
    } else {
        None
    }
}

fn open_with(
    url: &str,
    probe: &dyn crate::scope::Probe,
    spawn: impl FnOnce(&str) -> io::Result<()>,
) -> io::Result<()> {
    if let Some(reason) = refusal(probe) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
    }
    spawn(url)
}

fn spawn(url: &str) -> io::Result<()> {
    let mut command = opener();
    open_ferry_core::process_limits::restore_in_child(&mut command);
    let mut child = command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(windows)]
fn opener() -> Command {
    let mut command = Command::new("rundll32");
    command.arg("url.dll,FileProtocolHandler");
    command
}

#[cfg(target_os = "macos")]
fn opener() -> Command {
    Command::new("open")
}

#[cfg(not(any(windows, target_os = "macos")))]
fn opener() -> Command {
    Command::new("xdg-open")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::os_service::Platform;

    #[test]
    fn privileged_browser_requests_never_reach_the_spawner() {
        for privilege in [Ok(true), Err("token unavailable".into())] {
            let mut probe = crate::update::scope_probe::Fake::new(Platform::Linux, true);
            probe.privileged = privilege;
            let error = open_with("https://example.invalid/login", &probe, |_| {
                panic!("a privileged login must not spawn a browser")
            })
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(error.to_string().contains("administrator or root rights"));
            assert!(error.to_string().contains("Open the URL yourself"));
        }
    }

    #[test]
    fn unprivileged_and_container_browser_requests_keep_the_opener() {
        for container in [false, true] {
            let mut probe = crate::scope::tests::Fake::new(Platform::Linux, container);
            probe.container = container;
            let called = std::cell::Cell::new(false);
            open_with("https://example.invalid/login", &probe, |url| {
                assert_eq!(url, "https://example.invalid/login");
                called.set(true);
                Ok(())
            })
            .unwrap();
            assert!(called.get());
        }
    }
}
