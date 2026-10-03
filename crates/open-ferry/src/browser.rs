// Ported from CLIProxyAPI internal/browser/browser.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Opening a URL in the user's browser, for the OAuth logins.
//!
//! Deviations from upstream:
//! - It runs the platform's own opener (`rundll32 url.dll,FileProtocolHandler`
//!   on Windows, `open` on macOS, `xdg-open` elsewhere) and nothing else.
//!   Upstream tries a list of fallbacks, and checks a browser is there by
//!   opening `about:blank`.

use std::io;
use std::process::{Command, Stdio};

/// Opens `url` in the default browser, without waiting for it.
pub fn open(url: &str) -> io::Result<()> {
    let mut command = opener();
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
