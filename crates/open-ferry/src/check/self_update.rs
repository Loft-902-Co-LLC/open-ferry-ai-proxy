//! `check`'s `self-update` finding: whether open-ferry looks for releases
//! of itself, as `self-update` in the config and `OPEN_FERRY_SELF_UPDATE`
//! say, and what it does when one is out. It makes no request.
//!
//! - Off: ok, and it says that no update request is made and that
//!   `open-ferry update` still works when run.
//! - On or notify-only in a build that trusts no release key: a warning,
//!   as nothing can be checked, so nothing is updated.
//! - An install that doesn't update itself (see
//!   `open_ferry_update::install`): ok, saying it only tells of releases,
//!   and why.
//! - Otherwise ok, with how often it looks.
//!
//! A note on the settings, such as an `OPEN_FERRY_SELF_UPDATE` that can't
//! be used, is a warning of its own.

use std::path::Path;
use std::time::Duration;

use open_ferry_core::config::Config;
use open_ferry_update::install::{self, RealSystem, System};
use open_ferry_update::{
    Install, ModeSource, NotSelfUpdating, ReleaseKeys, SelfUpdateMode, Settings,
};

use crate::scope;

use super::{Environment, Finding};

const CHECK: &str = "self-update";

/// What the finding reads besides the config.
pub(super) struct Updates {
    /// `OPEN_FERRY_SELF_UPDATE`.
    pub(super) mode_env: Option<String>,
    /// Whether this build trusts a release key.
    pub(super) trusts_key: bool,
    /// Whether this install updates itself.
    pub(super) install: Install,
    /// Why this process may not use the update records.
    pub(super) refusal: Option<scope::Refusal>,
    /// Advice for a machine install whose records can't be read.
    pub(super) machine_hint: Option<String>,
}

impl Updates {
    /// This process's: its environment, its build and its install. It
    /// writes nothing.
    pub(super) fn current() -> Self {
        let system = RealSystem { write_probe: false };
        let (install, refusal, machine_hint) = match std::env::current_exe() {
            Ok(binary) => installation(&scope::Host, &system, &binary),
            Err(error) => (
                Install::NotifyOnly(NotSelfUpdating::UnknownBinary(error.to_string())),
                None,
                None,
            ),
        };
        Self {
            mode_env: std::env::var(open_ferry_update::MODE_ENV).ok(),
            trusts_key: ReleaseKeys::built_in().is_ok_and(|keys| !keys.is_empty()),
            install,
            refusal,
            machine_hint,
        }
    }
}

/// Reads the records only after scope lookup authorizes them. `system`
/// must assess permissions without writing a probe file.
fn installation(
    probe: &dyn scope::Probe,
    system: &dyn System,
    binary: &Path,
) -> (Install, Option<scope::Refusal>, Option<String>) {
    let home = binary
        .to_str()
        .ok_or_else(|| scope::Refusal {
            reason: "the installed binary isn't valid Unicode".to_owned(),
            remedy: None,
        })
        .and_then(|binary| scope::lookup(probe, binary));
    match home {
        Ok(home) => {
            let receipt = probe.receipt(&probe.platform().join(
                &home.data.root().to_string_lossy(),
                open_ferry_update::data_dir::RECEIPT_FILE,
            ));
            (
                install::assess_for(system, receipt, binary),
                None,
                home.machine_hint,
            )
        }
        Err(refusal) => (
            Install::NotifyOnly(NotSelfUpdating::NoReceipt),
            Some(refusal),
            None,
        ),
    }
}

/// The `self-update` finding, and one for each note on the settings.
pub(super) fn check_self_update(config: &Config, env: &Environment, findings: &mut Vec<Finding>) {
    let updates = &env.updates;
    if let Some(refusal) = &updates.refusal {
        findings.push(Finding::error(
            CHECK,
            &refusal.reason,
            refusal.remedy.as_deref().unwrap_or(""),
        ));
        return;
    }
    if let Some(hint) = &updates.machine_hint {
        findings.push(Finding::warning(CHECK, hint, ""));
    }
    let settings = Settings::resolve(&config.self_update, updates.mode_env.as_deref());
    for note in &settings.notes {
        findings.push(Finding::warning(
            CHECK,
            note.clone(),
            "fix the value it names",
        ));
    }
    let state = format!("{} ({})", settings.describe(), set_by(settings.source));
    let every = every(settings.check_every);
    let finding = match (settings.mode, &updates.install) {
        (SelfUpdateMode::Off, _) => Finding::ok(
            CHECK,
            format!(
                "{state}: open-ferry makes no update request; `open-ferry update` still works when you run it"
            ),
        ),
        _ if !updates.trusts_key => Finding::warning(
            CHECK,
            format!(
                "{state}, but this build trusts no release key, so it can't check a release and never updates"
            ),
            "use a release build, or turn updates off with `open-ferry update -mode off`",
        ),
        (_, Install::NotifyOnly(why)) => Finding::ok(
            CHECK,
            format!(
                "{state}, looking every {every}; this install only says when a release is out ({why})"
            ),
        ),
        (SelfUpdateMode::Notify, Install::SelfUpdating { .. }) => Finding::ok(
            CHECK,
            format!(
                "{state}, looking every {every}; it says when a release is out, and `open-ferry update` installs it"
            ),
        ),
        (SelfUpdateMode::Auto, Install::SelfUpdating { .. }) => Finding::ok(
            CHECK,
            format!(
                "{state}, looking every {every}; a newer release is downloaded, checked and made ready, and `open-ferry update` installs it"
            ),
        ),
    };
    findings.push(finding);
}

/// What set the mode.
fn set_by(source: ModeSource) -> String {
    match source {
        ModeSource::Default => "the default; `open-ferry update -mode off` turns it off".to_owned(),
        source => format!("set by {source}"),
    }
}

/// `interval` as a Go duration's hours, minutes or seconds.
fn every(interval: Duration) -> String {
    let seconds = interval.as_secs();
    if seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;
    use crate::check::Level;
    use crate::os_service::Platform;
    use crate::update::scope_probe::{Fake, Node};
    use std::io;
    use std::path::PathBuf;

    struct ReadOnlySystem;

    impl System for ReadOnlySystem {
        fn current_exe(&self) -> io::Result<PathBuf> {
            panic!("assessment uses the installed binary");
        }

        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            Ok(path.to_path_buf())
        }

        fn in_container(&self) -> bool {
            false
        }

        fn dir_writable(&self, _: &Path) -> bool {
            true
        }
    }

    fn findings(probe: &Fake, binary: &str) -> Vec<Finding> {
        let (install, refusal, machine_hint) =
            installation(probe, &ReadOnlySystem, Path::new(binary));
        let env = Environment {
            now: chrono::Utc::now(),
            build_date: None,
            dashboard_built: true,
            management_password: false,
            run_config_programs: false,
            updates: Updates {
                mode_env: None,
                trusts_key: true,
                install,
                refusal,
                machine_hint,
            },
        };
        let mut findings = Vec::new();
        check_self_update(&Config::default(), &env, &mut findings);
        findings
    }

    #[test]
    fn account_checks_read_unprivileged_records_and_refuse_untrusted_privileged_records() {
        for platform in [Platform::Windows, Platform::Linux, Platform::MacOs] {
            let mut probe = Fake::new(platform, false);
            let account = probe.account.clone().unwrap();
            let binary = platform.join(&account, "open-ferry");
            probe
                .file(&binary)
                .with_receipt(&account, &binary)
                .untrust(&account);
            assert_eq!(findings(&probe, &binary)[0].level, Level::Ok);
            probe.reads.borrow_mut().clear();
            probe.privileged = Ok(true);
            let found = findings(&probe, &binary);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].level, Level::Error);
            assert!(found[0].fix.contains(match platform {
                Platform::Windows => "not opened as administrator",
                Platform::Linux => "without sudo",
                Platform::MacOs => "macOS",
            }));
            assert!(
                probe.reads.borrow().is_empty(),
                "refused records aren't read"
            );
            assert!(probe.created.is_empty());
        }
    }

    #[test]
    fn machine_checks_read_the_machine_receipt_without_requiring_privilege() {
        for platform in [Platform::Windows, Platform::Linux] {
            for privileged in [false, true] {
                let mut probe = Fake::new(platform, privileged);
                let machine = scope::machine_root(&probe).unwrap().unwrap();
                let binary = platform.join(&machine, "open-ferry");
                probe.file(&binary).with_receipt(&machine, &binary);
                let found = findings(&probe, &binary);
                assert_eq!(found.len(), 1);
                assert_eq!(found[0].level, Level::Ok);
                assert!(
                    found[0]
                        .message
                        .contains("downloaded, checked and made ready")
                );
                let receipt = platform.join(&machine, open_ferry_update::data_dir::RECEIPT_FILE);
                assert!(probe.reads.borrow().iter().all(|path| path == &receipt));
                assert!(probe.created.is_empty(), "check never provisions");
            }
        }
    }

    #[test]
    fn unreadable_machine_records_produce_a_hint_finding() {
        for platform in [Platform::Windows, Platform::Linux] {
            let mut probe = Fake::new(platform, false);
            let machine = scope::machine_root(&probe).unwrap().unwrap();
            let receipt = platform.join(&machine, open_ferry_update::data_dir::RECEIPT_FILE);
            probe.node(&receipt, Node::Unreadable);
            let binary = platform.join(&machine, "open-ferry");
            let found = findings(&probe, &binary);
            assert_eq!(found.len(), 2);
            assert_eq!(found[0].level, Level::Warning);
            assert!(found[0].message.contains("whole machine"));
            assert!(found[0].message.contains(if platform == Platform::Windows {
                "terminal opened as administrator"
            } else {
                "sudo"
            }));
            assert!(probe.created.is_empty());
        }
    }
}
