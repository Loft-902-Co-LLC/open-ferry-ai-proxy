//! The service on macOS: a launchd job.
//!
//! By default it is an agent, `~/Library/LaunchAgents/<label>.plist`,
//! loaded into the user's GUI domain (`launchctl bootstrap gui/<uid>`): it
//! runs as them while they are logged in. With `-system` it is a daemon,
//! `/Library/LaunchDaemons/<label>.plist`, in the system domain, run as
//! root from boot. launchd starts it at load, and again when it exits with
//! a failure; what it prints goes to `~/Library/Logs/open-ferry.log`, or
//! `/Library/Logs/open-ferry.log` for the daemon.

use std::fmt::Write as _;

use super::{Cmd, Context, Definition, Platform, Step};

/// The job's label.
pub(super) const LABEL: &str = "io.github.loft-902-co-llc.open-ferry";

/// Where the job's definition goes.
pub(super) fn plist_path(context: &Context, system: bool) -> Result<String, String> {
    if system {
        return Ok(format!("/Library/LaunchDaemons/{LABEL}.plist"));
    }
    let dir = Platform::MacOs.join(context.home()?, "Library/LaunchAgents");
    Ok(Platform::MacOs.join(&dir, &format!("{LABEL}.plist")))
}

/// Where what the job prints goes.
pub(super) fn log_path(context: &Context, system: bool) -> Result<String, String> {
    if system {
        return Ok("/Library/Logs/open-ferry.log".to_owned());
    }
    Ok(Platform::MacOs.join(context.home()?, "Library/Logs/open-ferry.log"))
}

/// The domain the job is loaded into: the user's GUI session, or the
/// system's.
pub(super) fn domain(system: bool, uid: u32) -> String {
    if system {
        "system".to_owned()
    } else {
        format!("gui/{uid}")
    }
}

/// The property list that runs `definition`, printing to `log`.
pub(super) fn plist(definition: &Definition, log: &str, system: bool) -> String {
    let flag = if system { " -system" } else { "" };
    let mut plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Installed by `open-ferry service install{flag}`; `open-ferry service uninstall{flag}` removes it. -->
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
"#
    );
    for arg in [
        definition.exe.as_str(),
        "-config",
        definition.config.as_str(),
    ] {
        let _ = writeln!(plist, "\t\t<string>{}</string>", escape(arg));
    }
    let _ = write!(
        plist,
        "\t</array>
\t<key>WorkingDirectory</key>
\t<string>{dir}</string>
\t<key>RunAtLoad</key>
\t<true/>
\t<key>KeepAlive</key>
\t<dict>
\t\t<key>SuccessfulExit</key>
\t\t<false/>
\t</dict>
\t<key>StandardOutPath</key>
\t<string>{log}</string>
\t<key>StandardErrorPath</key>
\t<string>{log}</string>
</dict>
</plist>
",
        dir = escape(&definition.dir),
        log = escape(log),
    );
    plist
}

/// `text` escaped for XML.
pub(super) fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            c => escaped.push(c),
        }
    }
    escaped
}

fn service(domain: &str) -> String {
    format!("{domain}/{LABEL}")
}

/// Writes the job's definition at `path` and loads it into `domain`,
/// which starts it.
pub(super) fn install_steps(
    path: &str,
    domain: &str,
    definition: &Definition,
    log: &str,
) -> Vec<Step> {
    vec![
        Step::Write {
            path: path.to_owned(),
            text: plist(definition, log, domain == "system"),
            utf16: false,
        },
        // A job that was once disabled stays so until it is enabled, even
        // after it is removed and loaded again.
        Step::Run(Cmd::new("launchctl", &["enable"]).arg(service(domain))),
        Step::Run(Cmd::new("launchctl", &["bootstrap", domain]).arg(path)),
    ]
}

/// Unloads the job from `domain`, which stops it, and removes its
/// definition at `path`.
pub(super) fn uninstall_steps(path: &str, domain: &str) -> Vec<Step> {
    vec![
        Step::TryRun(Cmd::new("launchctl", &["bootout"]).arg(service(domain))),
        Step::Remove(path.to_owned()),
    ]
}

pub(super) fn status(domain: &str) -> Cmd {
    Cmd::new("launchctl", &["print"]).arg(service(domain))
}

pub(super) fn notes(system: bool, log: &str) -> String {
    if system {
        format!(
            "open-ferry is installed and started, as a launchd daemon: it starts at boot, and again when it fails.\nIts output: {log}"
        )
    } else {
        format!(
            "open-ferry is installed and started, as a launchd agent: it starts when you log in, and again when it fails.\nIts output: {log}"
        )
    }
}
