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

use std::time::Duration;

use open_ferry_core::config::Config;
use open_ferry_update::install::{self, RealSystem};
use open_ferry_update::receipt::Receipt;
use open_ferry_update::{DataDir, Install, ModeSource, ReleaseKeys, SelfUpdateMode, Settings};

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
}

impl Updates {
    /// This process's: its environment, its build and its install. It
    /// writes nothing.
    pub(super) fn current() -> Self {
        let system = RealSystem { write_probe: false };
        let receipt = match DataDir::for_this_user() {
            Ok(data) => Receipt::load(&data.receipt_file()),
            Err(_) => Ok(None),
        };
        Self {
            mode_env: std::env::var(open_ferry_update::MODE_ENV).ok(),
            trusts_key: ReleaseKeys::built_in().is_ok_and(|keys| !keys.is_empty()),
            install: install::assess(&system, receipt),
        }
    }
}

/// The `self-update` finding, and one for each note on the settings.
pub(super) fn check_self_update(config: &Config, env: &Environment, findings: &mut Vec<Finding>) {
    let updates = &env.updates;
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
