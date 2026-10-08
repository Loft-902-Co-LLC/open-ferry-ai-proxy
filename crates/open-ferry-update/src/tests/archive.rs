//! Not upstream's: taking the binary out of an archive, and refusing
//! archives that reach outside their directory, hold links or special
//! files, or are too large.

use super::support::*;
use crate::archive::{ArchiveError, extract_binary};
use crate::release::ArchiveKind;

const DIR: &str = "open-ferry-0.2.0-x86_64-unknown-linux-gnu";
const LIMIT: u64 = 1024 * 1024;

fn from_tar(entries: &[Entry]) -> Result<Vec<u8>, ArchiveError> {
    extract_binary(
        &tar_gz(entries),
        ArchiveKind::TarGz,
        DIR,
        "open-ferry",
        LIMIT,
    )
}

fn from_zip(entries: &[Entry]) -> Result<Vec<u8>, ArchiveError> {
    extract_binary(
        &zip(entries),
        ArchiveKind::Zip,
        DIR,
        "open-ferry.exe",
        LIMIT,
    )
}

fn binary() -> Entry {
    file(&format!("{DIR}/open-ferry"), b"the binary")
}

#[test]
fn the_binary_is_taken_from_a_release_tar_gz() {
    let archive = release_archive("0.2.0", LINUX, b"the binary");
    let data = extract_binary(&archive, ArchiveKind::TarGz, DIR, "open-ferry", LIMIT).unwrap();
    assert_eq!(data, b"the binary");
}

#[test]
fn the_binary_is_taken_from_a_release_zip() {
    let archive = release_archive("0.2.0", WINDOWS, b"the exe");
    let dir = "open-ferry-0.2.0-x86_64-pc-windows-msvc";
    let data = extract_binary(&archive, ArchiveKind::Zip, dir, "open-ferry.exe", LIMIT).unwrap();
    assert_eq!(data, b"the exe");
}

#[test]
fn a_leading_dot_slash_is_allowed() {
    let data = from_tar(&[
        Entry::Dir("./".into()),
        file(&format!("./{DIR}/open-ferry"), b"dotted"),
    ])
    .unwrap();
    assert_eq!(data, b"dotted");
}

#[test]
fn unsafe_paths_are_refused_in_a_tar() {
    for name in [
        format!("{DIR}/../../../.profile"),
        "../open-ferry".to_owned(),
        "/usr/local/bin/open-ferry".to_owned(),
        format!("{DIR}\\..\\open-ferry"),
        "C:/Windows/open-ferry.exe".to_owned(),
        format!("{DIR}/open-ferry:stream"),
    ] {
        let result = from_tar(&[binary(), file(&name, b"x")]);
        assert!(
            matches!(result, Err(ArchiveError::UnsafePath(_))),
            "{name}: {result:?}"
        );
    }
}

#[test]
fn unsafe_paths_are_refused_in_a_zip() {
    for name in ["../open-ferry.exe", "/open-ferry.exe", "C:/open-ferry.exe"] {
        let result = from_zip(&[
            file(&format!("{DIR}/open-ferry.exe"), b"exe"),
            file(name, b"x"),
        ]);
        assert!(
            matches!(result, Err(ArchiveError::UnsafePath(_))),
            "{name}: {result:?}"
        );
    }
}

#[test]
fn links_and_special_files_are_refused() {
    let cases = [
        Entry::Symlink(format!("{DIR}/open-ferry"), "/bin/sh".into()),
        Entry::Symlink(format!("{DIR}/README.md"), "../../etc/passwd".into()),
        Entry::Hardlink(format!("{DIR}/LICENSE"), "/etc/shadow".into()),
        Entry::CharDevice(format!("{DIR}/tty")),
    ];
    for entry in cases {
        let result = from_tar(&[binary(), entry]);
        assert!(
            matches!(result, Err(ArchiveError::NotAFile(_))),
            "{result:?}"
        );
    }
    let result = from_zip(&[
        file(&format!("{DIR}/open-ferry.exe"), b"exe"),
        Entry::Symlink(format!("{DIR}/link"), "../../x".into()),
    ]);
    assert!(
        matches!(result, Err(ArchiveError::NotAFile(_))),
        "{result:?}"
    );
}

#[test]
fn an_oversize_entry_is_refused() {
    let big = vec![0u8; 4096];
    let archive = tar_gz(&[binary(), file(&format!("{DIR}/LICENSE"), &big)]);
    let result = extract_binary(&archive, ArchiveKind::TarGz, DIR, "open-ferry", 1024);
    assert!(
        matches!(result, Err(ArchiveError::TooLarge(_))),
        "{result:?}"
    );
    let archive = zip(&[file(&format!("{DIR}/open-ferry.exe"), &big)]);
    let result = extract_binary(&archive, ArchiveKind::Zip, DIR, "open-ferry.exe", 1024);
    assert!(
        matches!(result, Err(ArchiveError::TooLarge(_))),
        "{result:?}"
    );
}

#[test]
fn a_missing_or_repeated_binary_is_refused() {
    let result = from_tar(&[file(&format!("{DIR}/LICENSE"), b"MIT")]);
    assert!(
        matches!(result, Err(ArchiveError::Missing(_))),
        "{result:?}"
    );
    // In another directory isn't in this one.
    let result = from_tar(&[file(
        "open-ferry-0.1.0-x86_64-unknown-linux-gnu/open-ferry",
        b"x",
    )]);
    assert!(
        matches!(result, Err(ArchiveError::Missing(_))),
        "{result:?}"
    );
    let result = from_tar(&[binary(), file(&format!("./{DIR}/open-ferry"), b"again")]);
    assert!(
        matches!(result, Err(ArchiveError::Repeated(_))),
        "{result:?}"
    );
}

#[test]
fn something_that_isnt_an_archive_is_refused() {
    for kind in [ArchiveKind::TarGz, ArchiveKind::Zip] {
        let result = extract_binary(b"not an archive", kind, DIR, "open-ferry", LIMIT);
        assert!(
            matches!(result, Err(ArchiveError::Corrupt(_))),
            "{result:?}"
        );
    }
}
