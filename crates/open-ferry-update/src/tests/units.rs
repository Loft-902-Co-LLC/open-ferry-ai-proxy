//! Not upstream's: the keys file, reading a release from SHA256SUMS,
//! the settings, the data directory, the state, the receipt, whether an
//! install updates itself, and the switch.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use open_ferry_core::config::{SelfUpdate, SelfUpdateMode};
use semver::Version;

use super::support::*;
use crate::data_dir::DataDir;
use crate::install::{self, Install, NotSelfUpdating};
use crate::keys::{KeyError, MAX_KEYS, ReleaseKeys, VerifyError};
use crate::receipt::{Receipt, ReceiptError};
use crate::release::{self, ReleaseError, Verdict};
use crate::runner::{ProcessRunner, Runner};
use crate::settings::{self, MIN_CHECK_EVERY, ModeSource, Settings};
use crate::state::State;
use crate::switch::{ReplaceOnDisk, Switch, SwitchPlan};

// The keys file.

#[test]
fn the_built_in_keys_file_parses() {
    let keys = ReleaseKeys::built_in().unwrap();
    assert!(keys.len() <= MAX_KEYS);
}

#[test]
fn a_keys_file_skips_comments_and_minisigs_own_line() {
    let key = TestKey::new();
    let text = format!(
        "# open-ferry's release keys\n\nuntrusted comment: minisign public key\n{}\n",
        key.public()
    );
    let keys = ReleaseKeys::parse(&text).unwrap();
    assert_eq!(keys.len(), 1);
    assert!(ReleaseKeys::parse("# none yet\n").unwrap().is_empty());
}

#[test]
fn a_bad_keys_file_is_refused() {
    let (a, b, c) = (TestKey::new(), TestKey::new(), TestKey::new());
    let three = format!("{}\n{}\n{}\n", a.public(), b.public(), c.public());
    assert_eq!(
        ReleaseKeys::parse(&three).unwrap_err(),
        KeyError::TooMany { count: 3 }
    );
    let twice = format!("{}\n# again\n{}\n", a.public(), a.public());
    assert_eq!(
        ReleaseKeys::parse(&twice).unwrap_err(),
        KeyError::Repeated { line: 3 }
    );
    assert_eq!(
        ReleaseKeys::parse("# keys\nRWQnot-a-key\n").unwrap_err(),
        KeyError::NotAKey { line: 2 }
    );
}

#[test]
fn no_key_trusts_no_release() {
    let key = TestKey::new();
    let signature = key.sign(b"data", "open-ferry 0.2.0 SHA256SUMS");
    let error = ReleaseKeys::none().verify(b"data", &signature).unwrap_err();
    assert_eq!(error, VerifyError::NoTrustedKey);
    assert!(error.to_string().contains("trusts no release key"));
    assert_eq!(
        trusting(&[&key]).verify(b"data", &signature).unwrap(),
        "open-ferry 0.2.0 SHA256SUMS"
    );
}

// SHA256SUMS, read as install.sh reads it.

const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn read(sums: &str, comment: &str, target: &str) -> Result<release::Release, ReleaseError> {
    release::read(sums.as_bytes(), comment, target)
}

#[test]
fn a_release_is_read_as_install_sh_reads_it() {
    let upper = HASH.to_ascii_uppercase();
    let sums = format!(
        "{HASH}  open-ferry-0.2.0-aarch64-apple-darwin.tar.gz\n\
         {upper}\t*open-ferry-0.2.0-x86_64-unknown-linux-gnu.tar.gz\n\
         {HASH}  open-ferry-0.2.0-x86_64-unknown-linux-musl.tar.gz\n\
         {HASH}  open-ferry-0.2.0-x86_64-pc-windows-msvc.zip\n"
    );
    let comment = "open-ferry 0.2.0 SHA256SUMS";
    let gnu = read(&sums, comment, LINUX).unwrap();
    assert_eq!(
        gnu.archive,
        "open-ferry-0.2.0-x86_64-unknown-linux-gnu.tar.gz"
    );
    assert_eq!(gnu.version, "0.2.0");
    assert_eq!(gnu.semver, Version::new(0, 2, 0));
    assert_eq!(gnu.sha256, HASH);
    let musl = read(&sums, comment, MUSL).unwrap();
    assert_eq!(
        musl.archive,
        "open-ferry-0.2.0-x86_64-unknown-linux-musl.tar.gz"
    );
    let windows = read(&sums, comment, WINDOWS).unwrap();
    assert_eq!(
        windows.archive,
        "open-ferry-0.2.0-x86_64-pc-windows-msvc.zip"
    );
    assert!(matches!(
        read(&sums, comment, "riscv64gc-unknown-linux-gnu"),
        Err(ReleaseError::NoArchive { .. })
    ));
}

#[test]
fn a_list_naming_another_version_is_refused() {
    let sums = format!(
        "{HASH}  open-ferry-0.2.0-x86_64-unknown-linux-gnu.tar.gz\n\
         {HASH}  open-ferry-0.1.0-aarch64-apple-darwin.tar.gz\n"
    );
    assert!(matches!(
        read(&sums, "open-ferry 0.2.0 SHA256SUMS", LINUX),
        Err(ReleaseError::OtherVersion { .. })
    ));
    for comment in [
        "open-ferry 0.2.0",
        "open-ferry v0.2.0 SHA256SUMS",
        "open-ferry 02.0.0 SHA256SUMS",
        "timestamp:1 file:SHA256SUMS",
    ] {
        assert!(
            matches!(
                read(&sums, comment, LINUX),
                Err(ReleaseError::TrustedComment(_))
            ),
            "{comment}"
        );
    }
}

#[test]
fn a_bad_hash_or_version_is_refused() {
    let short = "open-ferry-0.2.0-x86_64-unknown-linux-gnu.tar.gz";
    assert!(matches!(
        read(
            &format!("abc123  {short}\n"),
            "open-ferry 0.2.0 SHA256SUMS",
            LINUX
        ),
        Err(ReleaseError::BadHash(_))
    ));
    let other = HASH.replace('0', "f");
    assert!(matches!(
        read(
            &format!("{HASH}  {short}\n{other}  {short}\n"),
            "open-ferry 0.2.0 SHA256SUMS",
            LINUX
        ),
        Err(ReleaseError::BadHash(_))
    ));
    assert!(matches!(
        read(
            &format!("{HASH}  open-ferry-0.2-x86_64-unknown-linux-gnu.tar.gz\n"),
            "open-ferry 0.2.0 SHA256SUMS",
            LINUX
        ),
        Err(ReleaseError::OtherVersion { .. })
    ));
    assert!(matches!(
        release::read(&[0xff, 0xfe], "open-ferry 0.2.0 SHA256SUMS", LINUX),
        Err(ReleaseError::NotText)
    ));
}

#[test]
fn versions_compare_by_semver_and_prereleases_are_ignored() {
    let v = |text: &str| Version::parse(text).unwrap();
    assert_eq!(release::compare("0.1.0", &v("0.2.0")), Verdict::Newer);
    assert_eq!(release::compare("0.9.0", &v("0.10.0")), Verdict::Newer);
    assert_eq!(release::compare("0.2.0", &v("0.2.0")), Verdict::Same);
    assert_eq!(release::compare("0.3.0", &v("0.2.0")), Verdict::Older);
    assert_eq!(
        release::compare("0.1.0", &v("0.2.0-rc.1")),
        Verdict::Prerelease
    );
    assert_eq!(release::compare("0.2.0-rc.1", &v("0.2.0")), Verdict::Newer);
    assert_eq!(
        release::compare("0.2.0+build.5", &v("0.2.0")),
        Verdict::Same
    );
    assert_eq!(
        release::compare("not a version", &v("0.0.1")),
        Verdict::Newer
    );
    for (text, valid) in [
        ("1.2.3", true),
        ("1.2.3-rc.1", true),
        ("0.0.0", true),
        ("01.2.3", false),
        ("1.2", false),
        ("1.2.3-", false),
        ("1.2.3-rc_1", false),
        ("v1.2.3", false),
    ] {
        assert_eq!(release::valid_version(text), valid, "{text}");
    }
}

// The settings.

fn section(mode: &str, every: &str) -> SelfUpdate {
    SelfUpdate {
        mode: mode.to_owned(),
        check_every: every.to_owned(),
    }
}

#[test]
fn the_mode_defaults_to_auto_and_the_environment_only_lowers_it() {
    let default = Settings::resolve(&section("", ""), None);
    assert_eq!(
        (default.mode, default.source),
        (SelfUpdateMode::Auto, ModeSource::Default)
    );
    assert_eq!(default.check_every, Duration::from_secs(6 * 3600));
    assert!(default.notes.is_empty());

    let config = Settings::resolve(&section("notify", ""), None);
    assert_eq!(
        (config.mode, config.source),
        (SelfUpdateMode::Notify, ModeSource::Config)
    );

    let lowered = Settings::resolve(&section("auto", ""), Some("off"));
    assert_eq!(
        (lowered.mode, lowered.source),
        (SelfUpdateMode::Off, ModeSource::Environment)
    );
    // It can't raise it.
    let kept = Settings::resolve(&section("off", ""), Some("auto"));
    assert_eq!(
        (kept.mode, kept.source),
        (SelfUpdateMode::Off, ModeSource::Config)
    );
    let kept = Settings::resolve(&section("notify", ""), Some("notify"));
    assert_eq!(kept.source, ModeSource::Config);
    // A typo is ignored with a note, and can't turn updates on.
    let typo = Settings::resolve(&section("off", ""), Some("of"));
    assert_eq!(typo.mode, SelfUpdateMode::Off);
    assert_eq!(typo.notes.len(), 1);
    assert!(typo.notes[0].contains(crate::MODE_ENV));
    assert_eq!(
        (default.describe(), config.describe(), lowered.describe()),
        ("on", "notify-only", "off")
    );
}

#[test]
fn the_interval_has_a_least() {
    let short = Settings::resolve(&section("", "10m"), None);
    assert_eq!(short.check_every, MIN_CHECK_EVERY);
    assert_eq!(short.notes.len(), 1);
    let long = Settings::resolve(&section("", "1h30m"), None);
    assert_eq!(long.check_every, Duration::from_secs(5400));
    assert!(long.notes.is_empty());
}

#[test]
fn each_wait_is_the_interval_give_or_take_a_tenth() {
    let interval = Duration::from_secs(100);
    assert_eq!(settings::spread(interval, 0), Duration::from_secs(90));
    assert_eq!(settings::spread(interval, 20_000), Duration::from_secs(110));
    for random in [1, 7_777, 19_999, u64::MAX] {
        let wait = settings::spread(interval, random);
        assert!(wait >= Duration::from_secs(90) && wait <= Duration::from_secs(110));
    }
    for _ in 0..20 {
        let wait = settings::jittered(interval);
        assert!(wait >= Duration::from_secs(90) && wait <= Duration::from_secs(110));
        let first = settings::between(Duration::from_secs(300), Duration::from_secs(600));
        assert!(first >= Duration::from_secs(300) && first <= Duration::from_secs(600));
    }
}

// The data directory.

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    move |name| {
        pairs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| OsString::from(value))
    }
}

#[test]
fn the_data_directory_follows_xdg_then_home_and_localappdata_on_windows() {
    let at = |pairs: &[(&str, &str)], windows| {
        DataDir::locate(vars(pairs), windows).map(|dir| dir.root().to_path_buf())
    };
    assert_eq!(
        at(&[("XDG_DATA_HOME", "/data"), ("HOME", "/home/me")], false).unwrap(),
        Path::new("/data").join("open-ferry")
    );
    // A relative XDG_DATA_HOME is ignored, as the spec says.
    assert_eq!(
        at(&[("XDG_DATA_HOME", "data"), ("HOME", "/home/me")], false).unwrap(),
        Path::new("/home/me").join(".local/share/open-ferry")
    );
    assert!(at(&[("HOME", "relative")], false).is_err());
    assert!(at(&[], false).is_err());
    assert_eq!(
        at(&[("LOCALAPPDATA", r"C:\Users\me\AppData\Local")], true).unwrap(),
        PathBuf::from(r"C:\Users\me\AppData\Local").join("open-ferry")
    );
    assert!(at(&[("LOCALAPPDATA", "Local"), ("HOME", "/home/me")], true).is_err());
}

#[test]
fn the_update_lock_is_held_until_dropped() {
    let temp = tempfile::tempdir().unwrap();
    let data = DataDir::at(temp.path().join("data"));
    let lock = data.try_lock().unwrap().expect("the lock");
    assert!(data.try_lock().unwrap().is_none());
    drop(lock);
    assert!(data.try_lock().unwrap().is_some());
}

#[test]
fn kept_versions_are_the_directories_in_versions() {
    let temp = tempfile::tempdir().unwrap();
    let data = DataDir::at(temp.path());
    assert!(data.kept_versions().is_empty());
    fs::create_dir_all(data.version_dir("0.2.0")).unwrap();
    fs::create_dir_all(data.version_dir("0.1.0")).unwrap();
    fs::write(data.versions().join("stray"), "").unwrap();
    assert_eq!(data.kept_versions(), ["0.1.0", "0.2.0"]);
}

// The state.

#[test]
fn the_state_is_written_whole_and_read_back() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("update-state.json");
    assert_eq!(State::load(&path), State::default());
    let mut state = State {
        latest: Some("0.2.0".into()),
        staged: Some("0.2.0".into()),
        ..State::default()
    };
    for n in 0..12 {
        state.mark_failed(&format!("0.0.{n}"));
    }
    state.mark_failed("0.0.11");
    assert_eq!(state.failed.len(), 10);
    assert_eq!(state.failed.first().map(String::as_str), Some("0.0.2"));
    assert!(state.has_failed("0.0.11") && !state.has_failed("0.0.1"));
    state.save(&path).unwrap();
    let loaded = State::load(&path);
    assert_eq!(loaded.format, crate::state::FORMAT);
    assert_eq!(loaded.staged, state.staged);
    assert_eq!(loaded.failed, state.failed);
    let names: Vec<_> = fs::read_dir(temp.path()).unwrap().collect();
    assert_eq!(names.len(), 1, "no temporary file is left");
    fs::write(&path, "{ not json").unwrap();
    assert_eq!(State::load(&path), State::default());
}

// The receipt.

#[test]
fn a_receipt_is_read_or_refused() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("install-receipt.json");
    assert_eq!(Receipt::load(&path), Ok(None));
    fs::write(
        &path,
        r#"{"format":1,"installer":"install.ps1","version":"0.1.0","binary":"C:\\Users\\me\\.open-ferry\\bin\\open-ferry.exe","target":"x86_64-pc-windows-msvc","installed_at":"2026-10-08T12:00:00Z"}"#,
    )
    .unwrap();
    let receipt = Receipt::load(&path).unwrap().unwrap();
    assert_eq!(receipt.installer, "install.ps1");
    assert_eq!(
        receipt.binary,
        r"C:\Users\me\.open-ferry\bin\open-ferry.exe"
    );
    for bad in [
        "not json",
        r#"{"format":2,"binary":"/x"}"#,
        r#"{"format":1,"binary":""}"#,
    ] {
        fs::write(&path, bad).unwrap();
        assert!(
            matches!(Receipt::load(&path), Err(ReceiptError::Malformed(_))),
            "{bad}"
        );
    }
    fs::write(&path, vec![b' '; 17 * 1024]).unwrap();
    assert!(matches!(
        Receipt::load(&path),
        Err(ReceiptError::Malformed(_))
    ));
}

// Whether an install updates itself.

struct Machine {
    _temp: tempfile::TempDir,
    binary: PathBuf,
    data: DataDir,
}

fn machine(name: &str) -> Machine {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let binary = bin.join(name);
    fs::write(&binary, "binary").unwrap();
    let data = DataDir::at(temp.path().join("data"));
    Machine {
        _temp: temp,
        binary,
        data,
    }
}

fn assess(machine: &Machine, exe: &Path, container: bool, writable: bool) -> Install {
    let system = FakeSystem {
        exe: exe.to_path_buf(),
        container,
        writable,
    };
    install::assess(&system, Receipt::load(&machine.data.receipt_file()))
}

#[test]
fn only_the_binary_the_installer_wrote_updates_itself() {
    let m = machine("open-ferry");
    // No receipt: a package manager's, a hand copy, a build.
    assert_eq!(
        assess(&m, &m.binary, false, true),
        Install::NotifyOnly(NotSelfUpdating::NoReceipt)
    );
    write_receipt(&m.data, &m.binary);
    let install = assess(&m, &m.binary, false, true);
    assert_eq!(
        install,
        Install::SelfUpdating {
            binary: fs::canonicalize(&m.binary).unwrap()
        }
    );
    assert!(install.can_update_itself());
    // In a container, even with a receipt.
    assert_eq!(
        assess(&m, &m.binary, true, true),
        Install::NotifyOnly(NotSelfUpdating::Container)
    );
    // A directory it can't write.
    assert!(matches!(
        assess(&m, &m.binary, false, false),
        Install::NotifyOnly(NotSelfUpdating::ReadOnly(_))
    ));
    // Another copy of the binary.
    let copy = m.binary.with_file_name("copy");
    fs::create_dir_all(&copy).unwrap();
    let copy = copy.join("open-ferry");
    fs::write(&copy, "binary").unwrap();
    let install = assess(&m, &copy, false, true);
    assert_eq!(
        install.why_not().map(NotSelfUpdating::code),
        Some("other-binary")
    );
    // A running binary that can't be found.
    assert_eq!(
        assess(&m, &m.binary.with_file_name("gone"), false, true)
            .why_not()
            .map(NotSelfUpdating::code),
        Some("unknown-binary")
    );
    fs::write(m.data.receipt_file(), "{").unwrap();
    assert_eq!(
        assess(&m, &m.binary, false, true)
            .why_not()
            .map(NotSelfUpdating::code),
        Some("bad-receipt")
    );
}

#[test]
fn migrates_drop_in_copy_doesnt_update_itself() {
    let m = machine("cli-proxy-api");
    write_receipt(&m.data, &m.binary);
    let install = assess(&m, &m.binary, false, true);
    assert_eq!(
        install.why_not().map(NotSelfUpdating::code),
        Some("other-name")
    );
    assert!(
        install
            .why_not()
            .unwrap()
            .to_string()
            .contains("cli-proxy-api")
    );
}

// Not upstream's: migrate's drop-in on Linux and macOS is a symbolic link to
// the installed open-ferry. The running path is resolved, so it is the
// installed binary and updates itself.
#[cfg(unix)]
#[test]
fn migrates_drop_in_symlink_updates_the_installed_binary() {
    let m = machine("open-ferry");
    write_receipt(&m.data, &m.binary);
    let other = m.binary.with_file_name("elsewhere");
    fs::create_dir_all(&other).unwrap();
    let link = other.join("cli-proxy-api");
    std::os::unix::fs::symlink(&m.binary, &link).unwrap();
    assert_eq!(
        assess(&m, &link, false, true),
        Install::SelfUpdating {
            binary: fs::canonicalize(&m.binary).unwrap()
        }
    );
}

#[test]
fn a_container_is_recognized_by_its_files_and_variables() {
    let none = |_: &Path| false;
    assert!(!install::detect_container(none, vars(&[])));
    assert!(install::detect_container(
        |path: &Path| path == Path::new("/.dockerenv"),
        vars(&[])
    ));
    assert!(install::detect_container(
        |path: &Path| path == Path::new("/run/.containerenv"),
        vars(&[])
    ));
    assert!(install::detect_container(
        none,
        vars(&[("container", "podman")])
    ));
    assert!(!install::detect_container(none, vars(&[("container", "")])));
    assert!(install::detect_container(
        none,
        vars(&[("KUBERNETES_SERVICE_HOST", "10.0.0.1")])
    ));
}

// The switch, in a temporary directory.

#[test]
fn a_switch_replaces_the_binary_and_keeps_the_old_one() {
    let temp = tempfile::tempdir().unwrap();
    let installed = temp.path().join("bin").join("open-ferry");
    fs::create_dir_all(installed.parent().unwrap()).unwrap();
    fs::write(&installed, "old").unwrap();
    let new = temp.path().join("new");
    fs::write(&new, "new").unwrap();
    let keep = temp
        .path()
        .join("versions")
        .join("0.1.0")
        .join("open-ferry");
    let plan = SwitchPlan {
        from: "0.1.0".into(),
        to: "0.2.0".into(),
        new_binary: new.clone(),
        installed: installed.clone(),
        keep_installed_at: Some(keep.clone()),
    };
    ReplaceOnDisk.switch(&plan).unwrap();
    assert_eq!(fs::read_to_string(&installed).unwrap(), "new");
    assert_eq!(fs::read_to_string(&keep).unwrap(), "old");
    let names: Vec<String> = fs::read_dir(installed.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["open-ferry"]);

    // A missing new binary leaves the installed one as it was.
    let broken = SwitchPlan {
        new_binary: temp.path().join("missing"),
        keep_installed_at: None,
        ..plan
    };
    assert!(ReplaceOnDisk.switch(&broken).is_err());
    assert_eq!(fs::read_to_string(&installed).unwrap(), "new");
    assert_eq!(
        fs::read_dir(installed.parent().unwrap()).unwrap().count(),
        1
    );
}

#[tokio::test]
async fn a_binary_that_doesnt_start_fails_its_version_run() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("open-ferry-missing");
    let error = ProcessRunner
        .version(&missing, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(error.contains("doesn't start"), "{error}");
}
