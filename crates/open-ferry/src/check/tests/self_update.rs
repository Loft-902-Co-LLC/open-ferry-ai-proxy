//! Not upstream's: the `self-update` finding, for each mode and what set
//! it, a build without a release key, an install that doesn't update
//! itself, and the settings' notes.

use open_ferry_update::{Install, NotSelfUpdating};

use super::*;

/// The findings for a config with `self-update.mode` set to `mode` and
/// `self-update.check-every` to `every`, in `env`.
fn check_with(mode: &str, every: &str, env: &Environment) -> Vec<Finding> {
    let mut config = Config::default();
    config.self_update.mode = mode.to_owned();
    config.self_update.check_every = every.to_owned();
    let mut findings = Vec::new();
    crate::check::self_update::check_self_update(&config, env, &mut findings);
    findings
}

/// `env()` with `OPEN_FERRY_SELF_UPDATE` set to `value`.
fn with_mode_env(value: &str) -> Environment {
    let mut env = env();
    env.updates.mode_env = Some(value.to_owned());
    env
}

#[test]
fn on_by_default_says_how_often_and_how_to_turn_it_off() {
    let findings = check_with("", "", &env());

    assert_eq!(levels(&findings), [(Level::Ok, "self-update")]);
    assert_eq!(
        findings[0].message,
        "on (the default; `open-ferry update -mode off` turns it off), looking every 6h; a newer release is downloaded, checked and made ready, and `open-ferry update` installs it"
    );
}

#[test]
fn off_makes_no_request_and_leaves_the_command() {
    let findings = check_with("off", "", &env());

    assert_eq!(levels(&findings), [(Level::Ok, "self-update")]);
    assert_eq!(
        findings[0].message,
        "off (set by self-update.mode in the config): open-ferry makes no update request; `open-ferry update` still works when you run it"
    );
}

#[test]
fn the_environment_can_turn_it_down() {
    let findings = check_with("auto", "12h", &with_mode_env("notify"));

    assert_eq!(levels(&findings), [(Level::Ok, "self-update")]);
    assert!(
        findings[0].message.starts_with(
            "notify-only (set by OPEN_FERRY_SELF_UPDATE in the environment), looking every 12h; it says when a release is out"
        ),
        "{}",
        findings[0].message
    );

    let findings = check_with("", "", &with_mode_env("off"));
    assert!(
        findings[0]
            .message
            .starts_with("off (set by OPEN_FERRY_SELF_UPDATE in the environment)"),
        "{}",
        findings[0].message
    );
}

#[test]
fn a_build_without_a_release_key_is_a_warning_unless_updates_are_off() {
    let mut env = env();
    env.updates.trusts_key = false;

    let found = check_with("", "", &env);
    assert_eq!(levels(&found), [(Level::Warning, "self-update")]);
    assert!(
        found[0].message.contains("trusts no release key"),
        "{}",
        found[0].message
    );
    assert!(
        found[0].fix.contains("`open-ferry update -mode off`"),
        "{}",
        found[0].fix
    );

    let found = check_with("off", "", &env);
    assert_eq!(levels(&found), [(Level::Ok, "self-update")]);
}

#[test]
fn an_install_that_doesnt_update_itself_only_hears_of_releases() {
    let mut env = env();
    env.updates.install = Install::NotifyOnly(NotSelfUpdating::Container);

    let findings = check_with("", "", &env);

    assert_eq!(levels(&findings), [(Level::Ok, "self-update")]);
    assert!(
        findings[0]
            .message
            .ends_with("this install only says when a release is out (open-ferry runs in a container; update the image (docker pull) instead)"),
        "{}",
        findings[0].message
    );
}

#[test]
fn notes_on_the_settings_are_warnings() {
    let findings = check_with("", "10m", &with_mode_env("sometimes"));

    assert_eq!(
        levels(&findings),
        [
            (Level::Warning, "self-update"),
            (Level::Warning, "self-update"),
            (Level::Ok, "self-update"),
        ]
    );
    assert!(
        findings[0]
            .message
            .contains("OPEN_FERRY_SELF_UPDATE is \"sometimes\""),
        "{}",
        findings[0].message
    );
    assert!(
        findings[1].message.contains("under the least of 1h"),
        "{}",
        findings[1].message
    );
    assert!(
        findings[2].message.contains("looking every 1h"),
        "{}",
        findings[2].message
    );
}

#[test]
fn the_interval_reads_as_the_config_writes_it() {
    for (every, reads) in [("2h", "2h"), ("90m", "90m"), ("3601s", "3601s")] {
        let found = check_with("notify", every, &env());
        assert!(
            found[0]
                .message
                .contains(&format!("looking every {reads};")),
            "{}",
            found[0].message
        );
    }
}
