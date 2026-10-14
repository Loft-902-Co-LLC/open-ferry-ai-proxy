use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use super::{Entry, SpawnEnvironment, SpawnGuard};

fn entry() -> Entry {
    Entry::from_auth(&open_ferry_core::auth::Auth::default())
}

fn inherited() -> Vec<(OsString, OsString)> {
    [
        "LANG",
        "LC_ALL",
        "LC_MESSAGES",
        "TZ",
        "HTTP_PROXY",
        "https_proxy",
        "NO_PROXY",
        "all_proxy",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CONFIG_DIR",
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "SystemRoot",
        "windir",
        "ComSpec",
        "SystemDrive",
        "ProgramData",
        "PATHEXT",
        "PATH",
        "TMPDIR",
        "TEMP",
        "TMP",
        "NODE_OPTIONS",
        "NODE_PATH",
        "BASH_ENV",
        "ENV",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "PYTHONPATH",
        "PERL5LIB",
        "RUBYOPT",
        "UNRELATED",
        "ANTHROPIC_API_KEY",
        "CLAUDECODE",
    ]
    .into_iter()
    .map(|name| (name.into(), "inherited".into()))
    .collect()
}

fn restricted() -> SpawnEnvironment {
    SpawnEnvironment::Restricted {
        path: "trusted-program;trusted-system".into(),
        temp: PathBuf::from("trusted-work/tmp"),
        variables: vec![
            ("HOME".into(), "trusted-home".into()),
            ("CLAUDE_CONFIG_DIR".into(), "trusted-account".into()),
        ],
    }
}

#[test]
fn restricted_environments_keep_only_allowed_names_and_checked_values() {
    let variables: BTreeMap<_, _> = entry()
        .environment(inherited(), &restricted())
        .into_iter()
        .collect();
    for name in [
        "LANG",
        "LC_ALL",
        "LC_MESSAGES",
        "TZ",
        "HTTP_PROXY",
        "https_proxy",
        "NO_PROXY",
        "all_proxy",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ] {
        assert_eq!(
            variables.get(&OsString::from(name)),
            Some(&OsString::from("inherited")),
            "{name}"
        );
    }
    for name in [
        "NODE_OPTIONS",
        "NODE_PATH",
        "BASH_ENV",
        "ENV",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "PYTHONPATH",
        "PERL5LIB",
        "RUBYOPT",
        "UNRELATED",
        "ANTHROPIC_API_KEY",
        "CLAUDECODE",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "SystemRoot",
        "windir",
        "ComSpec",
        "SystemDrive",
        "ProgramData",
        "PATHEXT",
    ] {
        assert!(!variables.contains_key(&OsString::from(name)), "{name}");
    }
    assert_eq!(
        variables[&OsString::from("PATH")],
        "trusted-program;trusted-system"
    );
    assert_eq!(variables[&OsString::from("HOME")], "trusted-home");
    assert_eq!(
        variables[&OsString::from("CLAUDE_CONFIG_DIR")],
        "trusted-account"
    );
    for name in if cfg!(windows) {
        vec!["TEMP", "TMP"]
    } else {
        vec!["TMPDIR"]
    } {
        assert_eq!(variables[&OsString::from(name)], "trusted-work/tmp");
    }
    for name in if cfg!(windows) {
        vec!["TMPDIR"]
    } else {
        vec!["TEMP", "TMP"]
    } {
        assert!(!variables.contains_key(&OsString::from(name)));
    }
}

#[test]
fn restricted_environments_keep_switches_but_drop_locale_and_timezone_paths() {
    for name in [
        "DISABLE_AUTOUPDATER",
        "DISABLE_TELEMETRY",
        "DISABLE_ERROR_REPORTING",
    ] {
        let input = vec![(name.into(), "1".into())];
        assert!(
            entry()
                .environment(input.clone(), &restricted())
                .contains(&input[0])
        );
    }
    for (name, value) in [
        ("LANG", "/srv/locale"),
        ("LC_ALL", "locale/path"),
        ("LC_MESSAGES", "locale/path"),
        ("TZ", "/srv/zone"),
        ("TZ", ":/srv/zone"),
        ("TZ", "../zone"),
        ("TZ", "zone..name"),
    ] {
        let input = vec![(name.into(), value.into())];
        assert!(
            !entry()
                .environment(input.clone(), &restricted())
                .contains(&input[0])
        );
        assert!(
            entry()
                .environment(input.clone(), &SpawnEnvironment::Inherited)
                .contains(&input[0])
        );
    }
    for name in ["LANG", "LC_ALL", "LC_MESSAGES", "TZ"] {
        let input = vec![(name.into(), "C:\\zone".into())];
        assert!(
            !entry()
                .environment(input.clone(), &restricted())
                .contains(&input[0])
        );
    }
    for (name, value) in [
        ("LANG", "en_US.UTF-8"),
        ("LC_ALL", "C"),
        ("TZ", "America/New_York"),
        ("TZ", ":Europe/Paris"),
    ] {
        let input = vec![(name.into(), value.into())];
        assert!(
            entry()
                .environment(input.clone(), &restricted())
                .contains(&input[0])
        );
    }
    for name in [
        "NODE_EXTRA_CA_CERTS",
        "SSL_CERT_FILE",
        "CLAUDE_CODE_GIT_BASH_PATH",
    ] {
        let input = vec![(name.into(), "unchecked".into())];
        assert!(
            !entry()
                .environment(input.clone(), &restricted())
                .contains(&input[0])
        );
        let mut environment = restricted();
        let SpawnEnvironment::Restricted { variables, .. } = &mut environment else {
            panic!("restricted")
        };
        variables.push((name.into(), "checked".into()));
        assert!(
            entry()
                .environment(input, &environment)
                .contains(&(name.into(), "checked".into()))
        );
    }
}

#[test]
fn restricted_accounts_follow_the_existing_entry_override_rules() {
    let configured = Entry {
        config_dir: "explicit-account".into(),
        ..entry()
    };
    let variables: BTreeMap<_, _> = configured
        .environment(inherited(), &restricted())
        .into_iter()
        .collect();
    assert!(!variables.contains_key(&OsString::from("CLAUDE_CODE_OAUTH_TOKEN")));
    assert_eq!(
        variables[&OsString::from("CLAUDE_CONFIG_DIR")],
        "explicit-account"
    );
    let empty = SpawnEnvironment::Restricted {
        variables: Vec::new(),
        path: "trusted-path".into(),
        temp: "trusted-temp".into(),
    };
    assert!(
        !entry()
            .environment(inherited(), &empty)
            .iter()
            .any(|(name, _)| name == "CLAUDE_CONFIG_DIR")
    );
}

#[test]
fn unprivileged_environments_keep_the_existing_scrubbing_rules() {
    for config_dir in ["", "explicit-account"] {
        let entry = Entry {
            config_dir: config_dir.into(),
            ..entry()
        };
        let input = inherited();
        let mut expected: Vec<_> = input
            .iter()
            .filter(|(name, _)| !super::settings::scrubbed(name, config_dir.is_empty()))
            .cloned()
            .collect();
        if !config_dir.is_empty() {
            expected.push(("CLAUDE_CONFIG_DIR".into(), config_dir.into()));
        }
        assert_eq!(
            entry.environment(input, &SpawnEnvironment::Inherited),
            expected
        );
    }
}

/// A tiny executable made in scratch; it never runs Claude or accesses the network.
fn fake_program(dir: &std::path::Path) -> PathBuf {
    let source = dir.join("fake.rs");
    std::fs::write(
        &source,
        r#"
fn main() {
    let names: Vec<_> = std::env::vars_os().map(|(name, _)| name).collect();
    assert!(std::env::var_os("PATH").unwrap() == "trusted-program");
    let temp_name = if cfg!(windows) { "TEMP" } else { "TMPDIR" };
    let temp = std::env::var_os(temp_name).expect("private temp");
    assert!(std::path::Path::new(&temp).is_dir());
    for name in ["NODE_OPTIONS", "BASH_ENV", "LD_PRELOAD", "UNRELATED", "RUBYOPT"] {
        assert!(!names.iter().any(|present| present == name));
    }
    if std::env::args().any(|arg| arg == "--version") {
        println!("2.1.259");
    } else if std::env::args().any(|arg| arg == "auth") {
        println!("{{\"loggedIn\":true,\"authMethod\":\"oauth_token\"}}");
    } else {
        println!("restricted-child-ok");
    }
}
"#,
    )
    .unwrap();
    let program = dir.join(if cfg!(windows) { "fake.exe" } else { "fake" });
    assert!(
        std::process::Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&program)
            .status()
            .unwrap()
            .success()
    );
    program
}

#[tokio::test]
async fn a_privileged_fake_guard_restricts_requests_versions_and_status_children() {
    let scratch = std::env::var_os("OFP_TEST_SCRATCH").expect("scratch directory");
    let dir = tempfile::tempdir_in(scratch).unwrap();
    let entry = Entry {
        command: fake_program(dir.path()).to_str().unwrap().into(),
        ..entry()
    };
    let root = dir.path().join("entries");
    let guard = SpawnGuard::new(|entry, _, root| {
        Ok(SpawnEnvironment::Restricted {
            path: "trusted-program".into(),
            temp: entry.work_dirs(root).1.join("tmp"),
            variables: Vec::new(),
        })
    });
    let slot = std::sync::Arc::new(tokio::sync::Semaphore::new(1))
        .acquire_owned()
        .await
        .unwrap();
    let mut running = super::process::spawn_guarded(
        &root,
        super::process::Invocation {
            entry: &entry,
            model: "claude",
            system: "",
            effort: None,
            max_output_tokens: None,
            thinking_budget: None,
            input: Vec::new(),
        },
        slot,
        Some(&guard),
    )
    .unwrap();
    assert_eq!(
        running.stdout.next_line().await.unwrap().unwrap(),
        b"restricted-child-ok"
    );
    assert!(running.child.wait().await.unwrap().success());
    assert_eq!(
        super::version::check_version_in(&entry, Some(&guard), &root).await,
        super::VersionCheck::Supported("2.1.259".into())
    );
    assert!(
        super::auth_status_guarded(&entry, &root, Some(&guard))
            .await
            .unwrap()
            .logged_in
    );
}
