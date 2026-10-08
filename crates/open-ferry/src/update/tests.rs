//! Not upstream's: `open-ferry update`'s tests, with a fake updater and a
//! fake prompt, so nothing is downloaded or replaced, and configs in
//! temporary directories.

use std::cell::RefCell;

use open_ferry_core::config::SelfUpdate;
use open_ferry_update::{NotSelfUpdating, VerifyError};
use serde_json::Value;

use super::*;

/// Where the fake install's binary is.
const BINARY: &str = "/opt/open-ferry/open-ferry";

/// An updater that changes nothing: it finds `latest`, and records what
/// it is asked.
struct Fake {
    /// The latest release, or `None` for a check that fails.
    latest: Option<Offer<()>>,
    /// The rollback, or `None` for none.
    plan: Option<RollbackPlan>,
    /// Whether the switch fails.
    switch_fails: bool,
    calls: RefCell<Vec<&'static str>>,
}

impl Fake {
    /// Finds `latest`, with `installed` installed, on an install that
    /// updates itself.
    fn finding(latest: &str, installed: &str) -> Self {
        Self {
            latest: Some(Offer {
                latest: latest.to_owned(),
                installed: installed.to_owned(),
                newer: latest > installed,
                failed_here: false,
                rolled_back: false,
                install: Install::SelfUpdating {
                    binary: PathBuf::from(BINARY),
                },
                found: (),
            }),
            plan: None,
            switch_fails: false,
            calls: RefCell::new(Vec::new()),
        }
    }

    /// A check that fails.
    fn failing() -> Self {
        Self {
            latest: None,
            ..Self::finding("0.1.0", "0.1.0")
        }
    }

    /// The same, on an install that doesn't update itself.
    fn not_self_updating(mut self) -> Self {
        if let Some(offer) = &mut self.latest {
            offer.install = Install::NotifyOnly(NotSelfUpdating::NoReceipt);
        }
        self
    }

    fn offer(&mut self) -> &mut Offer<()> {
        self.latest.as_mut().unwrap()
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.borrow().clone()
    }
}

impl Updates for Fake {
    type Found = ();

    fn status(&self, settings: &Settings) -> Status {
        Status {
            mode: settings.mode.as_str(),
            mode_source: settings.source.as_str(),
            updates: settings.describe(),
            check_every_seconds: settings.check_every.as_secs(),
            running_version: "0.1.0".to_owned(),
            installed_version: "0.1.0".to_owned(),
            restart_needed: false,
            target: "x86_64-unknown-linux-gnu".to_owned(),
            latest_version: None,
            update_available: false,
            staged_version: None,
            previous_version: None,
            failed_versions: Vec::new(),
            rolled_back_version: None,
            last_check: None,
            last_result: None,
            last_error: None,
            next_check: None,
            checking: false,
            can_update_itself: true,
            why_not: None,
            why_not_code: None,
            trusts_release_key: true,
            notes: settings.notes.clone(),
        }
    }

    async fn latest(&self) -> Result<Offer<()>, UpdateError> {
        self.calls.borrow_mut().push("latest");
        self.latest
            .clone()
            .ok_or(UpdateError::Verify(VerifyError::NoTrustedKey))
    }

    fn rollback_plan(&self) -> Result<RollbackPlan, UpdateError> {
        self.plan.clone().ok_or(UpdateError::NoPrevious)
    }

    async fn update(&self, (): ()) -> Result<SwitchRecord, UpdateError> {
        self.calls.borrow_mut().push("update");
        if self.switch_fails {
            return Err(UpdateError::Busy);
        }
        let offer = self.latest.as_ref().unwrap();
        Ok(SwitchRecord {
            from: offer.installed.clone(),
            to: offer.latest.clone(),
            how: "update".to_owned(),
            binary: BINARY.to_owned(),
            restart_needed: true,
            ..SwitchRecord::default()
        })
    }

    async fn rollback(&self) -> Result<SwitchRecord, UpdateError> {
        self.calls.borrow_mut().push("rollback");
        let plan = self.plan.as_ref().unwrap();
        Ok(SwitchRecord {
            from: plan.installed.clone(),
            to: plan.previous.clone(),
            how: "rollback".to_owned(),
            binary: BINARY.to_owned(),
            restart_needed: true,
            ..SwitchRecord::default()
        })
    }
}

/// A prompt that answers `answer` and records the questions.
struct Answer {
    answer: Option<bool>,
    asked: Vec<String>,
}

impl Prompt for Answer {
    fn confirm(&mut self, question: &str) -> Option<bool> {
        self.asked.push(question.to_owned());
        self.answer
    }
}

/// What a run printed and returned.
#[derive(Debug)]
struct Ran {
    code: u8,
    result: &'static str,
    out: String,
    err: String,
    asked: Vec<String>,
}

impl Ran {
    /// The JSON printed.
    fn json(&self) -> Value {
        serde_json::from_str(&self.out).unwrap_or_else(|e| panic!("{e}: {}", self.out))
    }
}

/// The settings for `self-update.mode: mode` and `OPEN_FERRY_SELF_UPDATE`
/// set to `env`.
fn settings(mode: &str, env: Option<&str>) -> Settings {
    Settings::resolve(
        &SelfUpdate {
            mode: mode.to_owned(),
            check_every: String::new(),
        },
        env,
    )
}

/// Runs `request` with `fake`, as `update` with `-yes` if `yes` and
/// `-json` if `json`, the prompt answering `answer`.
async fn go(
    request: Request,
    fake: &Fake,
    settings: &Settings,
    yes: bool,
    json: bool,
    answer: Option<bool>,
) -> Ran {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut prompt = Answer {
        answer,
        asked: Vec::new(),
    };
    let mut console = Console::new(json, &mut out, &mut err);
    console.mode_line(request, settings, Path::new("config.yaml"), true);
    let outcome = run(request, fake, settings, yes, &mut prompt, &mut console).await;
    let (code, result) = (outcome.code, outcome.result);
    let _ = console.finish(outcome);
    Ran {
        code,
        result,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
        asked: prompt.asked,
    }
}

#[tokio::test]
async fn check_says_when_it_is_up_to_date() {
    let fake = Fake::finding("0.1.0", "0.1.0");

    let ran = go(
        Request::Check,
        &fake,
        &settings("", None),
        false,
        false,
        None,
    )
    .await;

    assert_eq!((ran.code, ran.result), (0, "up-to-date"));
    assert_eq!(
        ran.out,
        "Automatic updates are on (the default).\nopen-ferry 0.1.0 is installed, the latest release.\n"
    );
    assert_eq!(fake.calls(), ["latest"]);
}

#[tokio::test]
async fn check_exits_with_3_when_a_newer_release_is_out() {
    let fake = Fake::finding("0.2.0", "0.1.0");

    let ran = go(
        Request::Check,
        &fake,
        &settings("", None),
        false,
        false,
        None,
    )
    .await;

    assert_eq!((ran.code, ran.result), (3, "update-available"));
    assert!(
        ran.out.contains(
            "open-ferry 0.2.0 is out (0.1.0 is installed). Run `open-ferry update` to install it.\n"
        ),
        "{}",
        ran.out
    );
    assert!(ran.asked.is_empty());
    assert_eq!(fake.calls(), ["latest"]);
}

#[tokio::test]
async fn check_json_is_one_object_with_the_mode_and_the_status() {
    let fake = Fake::finding("0.2.0", "0.1.0");

    let ran = go(
        Request::Check,
        &fake,
        &settings("notify", None),
        false,
        true,
        None,
    )
    .await;

    assert_eq!(ran.code, 3);
    let json = ran.json();
    assert_eq!(json["action"], "check");
    assert_eq!(json["result"], "update-available");
    assert_eq!(json["mode"], "notify");
    assert_eq!(json["mode_source"], "config");
    assert_eq!(json["updates"], "notify-only");
    assert_eq!(json["latest_version"], "0.2.0");
    assert_eq!(json["installed_version"], "0.1.0");
    assert_eq!(json["status"]["latest_version"], "0.2.0");
    assert_eq!(json["status"]["update_available"], true);
    assert_eq!(json["status"]["updates"], "notify-only");
    assert!(
        json["message"]
            .as_str()
            .unwrap()
            .starts_with("open-ferry 0.2.0 is out")
    );
    assert!(json.get("switch").is_none(), "{json}");
    assert!(ran.err.is_empty());
}

#[tokio::test]
async fn check_works_while_updates_are_off_and_says_so() {
    let fake = Fake::finding("0.2.0", "0.1.0");

    let ran = go(
        Request::Check,
        &fake,
        &settings("off", None),
        false,
        false,
        None,
    )
    .await;

    assert!(
        ran.out.starts_with(
            "Automatic updates are off (set by self-update.mode in config.yaml). Checking because you asked.\n"
        ),
        "{}",
        ran.out
    );
    assert_eq!(ran.code, 3);
    assert_eq!(fake.calls(), ["latest"]);
}

#[tokio::test]
async fn check_says_why_an_install_doesnt_update_itself() {
    let fake = Fake::finding("0.2.0", "0.1.0").not_self_updating();

    let ran = go(
        Request::Check,
        &fake,
        &settings("", None),
        false,
        false,
        None,
    )
    .await;

    assert_eq!((ran.code, ran.result), (3, "cannot-update"));
    assert!(
        ran.out
            .contains("but this install doesn't update itself: this binary wasn't installed by"),
        "{}",
        ran.out
    );
}

#[tokio::test]
async fn check_says_when_automatic_updates_skip_a_release() {
    let mut fake = Fake::finding("0.2.0", "0.1.0");
    fake.offer().failed_here = true;
    fake.offer().rolled_back = true;

    let ran = go(
        Request::Check,
        &fake,
        &settings("", None),
        false,
        false,
        None,
    )
    .await;

    assert!(
        ran.out.contains("didn't run on this machine before"),
        "{}",
        ran.out
    );
    assert!(ran.out.contains("It was rolled back from"), "{}", ran.out);
}

#[tokio::test]
async fn a_failed_check_exits_with_1_and_says_why_on_standard_error() {
    let fake = Fake::failing();

    let ran = go(
        Request::Check,
        &fake,
        &settings("", None),
        false,
        false,
        None,
    )
    .await;

    assert_eq!((ran.code, ran.result), (1, "error"));
    assert!(
        ran.err
            .starts_with("update: this build of open-ferry trusts no release key"),
        "{}",
        ran.err
    );
    assert_eq!(ran.out, "Automatic updates are on (the default).\n");

    let ran = go(
        Request::Update,
        &fake,
        &settings("", None),
        true,
        true,
        None,
    )
    .await;
    assert_eq!(ran.code, 1);
    let json = ran.json();
    assert_eq!(json["result"], "error");
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("trusts no release key")
    );
    assert!(ran.err.is_empty(), "{}", ran.err);
}

#[tokio::test]
async fn without_a_terminal_update_changes_nothing() {
    let fake = Fake::finding("0.2.0", "0.1.0");

    let ran = go(
        Request::Update,
        &fake,
        &settings("", None),
        false,
        false,
        None,
    )
    .await;

    assert_eq!((ran.code, ran.result), (3, "needs-confirmation"));
    assert_eq!(ran.asked, ["Install it?"]);
    assert_eq!(fake.calls(), ["latest"]);
    assert!(
        ran.out.contains(&format!(
            "open-ferry 0.2.0 is out (0.1.0 is installed at {}).",
            Path::new(BINARY).display()
        )),
        "{}",
        ran.out
    );
    assert!(ran.out.contains("runs it with --version"), "{}", ran.out);
    assert!(
        ran.out.contains("Run `open-ferry update -yes`"),
        "{}",
        ran.out
    );
}

#[tokio::test]
async fn update_json_without_yes_asks_nothing_and_changes_nothing() {
    let fake = Fake::finding("0.2.0", "0.1.0");

    let ran = go(
        Request::Update,
        &fake,
        &settings("", None),
        false,
        true,
        Some(true),
    )
    .await;

    assert_eq!(ran.code, 3);
    assert!(ran.asked.is_empty());
    assert_eq!(fake.calls(), ["latest"]);
    let json = ran.json();
    assert_eq!(json["action"], "update");
    assert_eq!(json["result"], "needs-confirmation");
}

#[tokio::test]
async fn a_declined_update_changes_nothing() {
    let fake = Fake::finding("0.2.0", "0.1.0");

    let ran = go(
        Request::Update,
        &fake,
        &settings("", None),
        false,
        false,
        Some(false),
    )
    .await;

    assert_eq!((ran.code, ran.result), (3, "declined"));
    assert_eq!(fake.calls(), ["latest"]);
    assert!(ran.out.ends_with("Nothing was changed.\n"), "{}", ran.out);
}

#[tokio::test]
async fn an_accepted_update_switches_and_says_how_to_restart_and_roll_back() {
    let fake = Fake::finding("0.2.0", "0.1.0");

    let ran = go(
        Request::Update,
        &fake,
        &settings("", None),
        false,
        false,
        Some(true),
    )
    .await;

    assert_eq!((ran.code, ran.result), (0, "updated"));
    assert_eq!(ran.asked, ["Install it?"]);
    assert_eq!(fake.calls(), ["latest", "update"]);
    assert!(
        ran.out.contains(&format!(
            "open-ferry 0.2.0 is installed at {BINARY} (it was 0.1.0)."
        )),
        "{}",
        ran.out
    );
    assert!(
        ran.out.contains("Restart open-ferry to run 0.2.0."),
        "{}",
        ran.out
    );
    assert!(
        ran.out
            .ends_with("`open-ferry update -rollback` puts 0.1.0 back.\n"),
        "{}",
        ran.out
    );
}

#[tokio::test]
async fn update_yes_asks_nothing_and_json_has_the_switch() {
    let fake = Fake::finding("0.2.0", "0.1.0");

    let ran = go(
        Request::Update,
        &fake,
        &settings("", None),
        true,
        true,
        None,
    )
    .await;

    assert_eq!(ran.code, 0);
    assert!(ran.asked.is_empty());
    let json = ran.json();
    assert_eq!(json["result"], "updated");
    assert_eq!(json["switch"]["from"], "0.1.0");
    assert_eq!(json["switch"]["to"], "0.2.0");
    assert_eq!(json["switch"]["how"], "update");
    assert_eq!(json["switch"]["restart_needed"], true);
}

#[tokio::test]
async fn a_manual_update_works_while_updates_are_off_and_says_so() {
    let fake = Fake::finding("0.2.0", "0.1.0");

    let ran = go(
        Request::Update,
        &fake,
        &settings("auto", Some("off")),
        true,
        false,
        None,
    )
    .await;

    assert_eq!((ran.code, ran.result), (0, "updated"));
    assert!(
        ran.out.starts_with(
            "Automatic updates are off (set by OPEN_FERRY_SELF_UPDATE in the environment). Updating because you asked.\n"
        ),
        "{}",
        ran.out
    );
}

#[tokio::test]
async fn update_refuses_on_an_install_that_doesnt_update_itself() {
    let fake = Fake::finding("0.2.0", "0.1.0").not_self_updating();

    let ran = go(
        Request::Update,
        &fake,
        &settings("", None),
        true,
        false,
        None,
    )
    .await;

    assert_eq!((ran.code, ran.result), (3, "cannot-update"));
    assert_eq!(fake.calls(), ["latest"]);
}

#[tokio::test]
async fn update_tries_a_failed_release_again_and_says_so() {
    let mut fake = Fake::finding("0.2.0", "0.1.0");
    fake.offer().failed_here = true;

    let ran = go(
        Request::Update,
        &fake,
        &settings("", None),
        true,
        false,
        None,
    )
    .await;

    assert!(
        ran.out
            .contains("It didn't run on this machine before; trying it again because you asked."),
        "{}",
        ran.out
    );
    assert_eq!(ran.code, 0);
}

#[tokio::test]
async fn update_when_up_to_date_changes_nothing() {
    let fake = Fake::finding("0.1.0", "0.1.0");

    let ran = go(
        Request::Update,
        &fake,
        &settings("", None),
        true,
        false,
        None,
    )
    .await;

    assert_eq!((ran.code, ran.result), (0, "up-to-date"));
    assert_eq!(fake.calls(), ["latest"]);
}

#[tokio::test]
async fn a_failed_switch_exits_with_1() {
    let mut fake = Fake::finding("0.2.0", "0.1.0");
    fake.switch_fails = true;

    let ran = go(
        Request::Update,
        &fake,
        &settings("", None),
        true,
        false,
        None,
    )
    .await;

    assert_eq!((ran.code, ran.result), (1, "error"));
    assert_eq!(ran.err, "update: another update is running\n");
}

#[tokio::test]
async fn rollback_puts_the_previous_version_back() {
    let mut fake = Fake::finding("0.2.0", "0.2.0");
    fake.plan = Some(RollbackPlan {
        previous: "0.1.0".to_owned(),
        installed: "0.2.0".to_owned(),
        binary: PathBuf::from(BINARY),
    });

    let ran = go(
        Request::Rollback,
        &fake,
        &settings("off", None),
        false,
        false,
        Some(true),
    )
    .await;

    assert_eq!((ran.code, ran.result), (0, "rolled-back"));
    assert_eq!(ran.asked, ["Roll back?"]);
    assert_eq!(fake.calls(), ["rollback"]);
    assert!(
        ran.out.contains(&format!(
            "open-ferry 0.1.0 is back at {BINARY} (it was 0.2.0)."
        )),
        "{}",
        ran.out
    );
}

#[tokio::test]
async fn rollback_without_a_terminal_changes_nothing() {
    let mut fake = Fake::finding("0.2.0", "0.2.0");
    fake.plan = Some(RollbackPlan {
        previous: "0.1.0".to_owned(),
        installed: "0.2.0".to_owned(),
        binary: PathBuf::from(BINARY),
    });

    let ran = go(
        Request::Rollback,
        &fake,
        &settings("", None),
        false,
        false,
        None,
    )
    .await;

    assert_eq!((ran.code, ran.result), (1, "needs-confirmation"));
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn rollback_without_an_earlier_version_exits_with_1() {
    let fake = Fake::finding("0.2.0", "0.2.0");

    let ran = go(
        Request::Rollback,
        &fake,
        &settings("", None),
        true,
        false,
        None,
    )
    .await;

    assert_eq!((ran.code, ran.result), (1, "error"));
    assert!(
        ran.err
            .starts_with("update: there is no earlier version to roll back to"),
        "{}",
        ran.err
    );
    assert!(fake.calls().is_empty());
}

/// Runs `-mode mode` on `path`, with `OPEN_FERRY_SELF_UPDATE` set to
/// `env`, and returns the exit code, the result and what was printed.
fn mode(
    path: &Path,
    mode: SelfUpdateMode,
    env: Option<&str>,
) -> (u8, &'static str, String, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut console = Console::new(false, &mut out, &mut err);
    let outcome = set_mode(path, mode, env, &mut console);
    let (code, result) = (outcome.code, outcome.result);
    let _ = console.finish(outcome);
    (
        code,
        result,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn mode_writes_the_setting_and_keeps_the_rest_of_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "# my proxy\nport: 8317 # the port\n").unwrap();

    let (code, result, out, err) = mode(&path, SelfUpdateMode::Off, None);

    assert_eq!((code, result), (0, "mode-set"), "{err}");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# my proxy\n"), "{text}");
    assert!(text.contains("# the port"), "{text}");
    let config = Config::load(&path).unwrap();
    assert_eq!(config.self_update.mode, "off");
    assert_eq!(config.port, 8317);
    assert!(
        out.contains("Automatic updates are off: open-ferry makes no update request. `open-ferry update` still works when you run it."),
        "{out}"
    );

    let (code, _, _, _) = mode(&path, SelfUpdateMode::Auto, None);
    assert_eq!(code, 0);
    assert_eq!(Config::load(&path).unwrap().self_update.mode, "auto");
}

#[test]
fn mode_says_when_the_environment_lowers_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "port: 8317\n").unwrap();

    let (code, _, out, _) = mode(&path, SelfUpdateMode::Auto, Some("notify"));

    assert_eq!(code, 0);
    assert!(
        out.contains("But OPEN_FERRY_SELF_UPDATE lowers it to notify"),
        "{out}"
    );
}

#[test]
fn mode_refuses_a_config_that_doesnt_exist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");

    let (code, result, out, err) = mode(&path, SelfUpdateMode::Off, None);

    assert_eq!((code, result), (1, "error"));
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("open-ferry init"), "{err}");
    assert!(!path.exists());
}

/// The options `args` give.
fn options(args: &[&str]) -> Options {
    let (options, rest) =
        flags::parse_with(&DEFINITIONS, args.iter().map(|arg| (*arg).to_owned())).unwrap();
    assert!(rest.is_empty(), "{rest:?}");
    options
}

#[test]
fn the_flags_make_one_request() {
    assert_eq!(request(&options(&[])), Ok(Request::Update));
    assert_eq!(request(&options(&["-check"])), Ok(Request::Check));
    assert_eq!(
        request(&options(&["--rollback", "--yes"])),
        Ok(Request::Rollback)
    );
    assert_eq!(
        request(&options(&["-mode", "OFF"])),
        Ok(Request::Mode(SelfUpdateMode::Off))
    );
    assert_eq!(
        request(&options(&["-mode=notify"])),
        Ok(Request::Mode(SelfUpdateMode::Notify))
    );
    assert!(request(&options(&["-check", "-rollback"])).is_err());
    assert!(request(&options(&["-check", "-mode", "off"])).is_err());
    assert!(request(&options(&["-mode", ""])).is_err());
    assert_eq!(
        request(&options(&["-mode", "sometimes"])),
        Err("-mode is \"sometimes\"; use off, notify or auto".to_owned())
    );
}

#[test]
fn the_config_is_the_flag_then_the_working_directory_then_the_installed_one() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let installed = home.join("config.yaml");

    assert_eq!(
        config_path("given.yaml", &work, Some(installed.clone())),
        PathBuf::from("given.yaml")
    );
    assert_eq!(
        config_path("", &work, Some(installed.clone())),
        work.join("config.yaml")
    );
    std::fs::write(&installed, "").unwrap();
    assert_eq!(config_path("", &work, Some(installed.clone())), installed);
    std::fs::write(work.join("config.yaml"), "").unwrap();
    assert_eq!(
        config_path("", &work, Some(installed)),
        work.join("config.yaml")
    );
}

#[test]
fn the_usage_says_how_to_turn_updates_off() {
    let usage = usage("open-ferry");
    assert!(usage.starts_with("Usage: open-ferry update [flags]\n"));
    assert!(usage.contains("Turn automatic updates off: open-ferry update -mode off\n"));
    assert!(usage.contains("\n  -mode string\n"));
    assert!(usage.ends_with("\n  -yes\n    \tInstall or roll back without asking\n"));
}
