//! Trust checks before agent commands read or write local files.

use std::path::Path;

use super::Failure;
use crate::scope::{self, Probe, State};

/// Checks a file before reading it, including when its path is resolved later.
pub(super) fn read(probe: &dyn Probe, path: &Path) -> Result<(), Failure> {
    if scope::authorize_file(probe, path)
        .map_err(|error| Failure::new("unsafe_file", error.to_string()))?
    {
        Ok(())
    } else {
        Err(Failure::new(
            "not_found",
            format!("{} doesn't exist", path.display()),
        ))
    }
}

/// Checks the parent and an existing target before creating or replacing a file.
pub(super) fn write(probe: &dyn Probe, path: &Path) -> Result<(), Failure> {
    optional_file(probe, path)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !scope::authorize_dir(probe, parent)
        .map_err(|error| Failure::new("unsafe_file", error.to_string()))?
    {
        return Err(Failure::new(
            "unsafe_file",
            format!(
                "the directory {} doesn't exist; nothing was changed",
                parent.display()
            ),
        )
        .hint("create the directory with trusted ownership and permissions, then try again"));
    }
    Ok(())
}

/// Checks every path the config writer may open, before it takes its lock.
pub(super) fn config_write(probe: &dyn Probe, path: &Path) -> Result<(), Failure> {
    write(probe, path)?;
    for suffix in [".bak", ".lock", ".sha256"] {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(suffix);
        optional_file(probe, &path.with_file_name(name))?;
    }
    Ok(())
}

/// Missing leaves can be created in the checked parent; existing ones must pass.
fn optional_file(probe: &dyn Probe, path: &Path) -> Result<(), Failure> {
    let existing = !matches!(probe.privileged(), Ok(false))
        && !probe.in_container()
        && path
            .to_str()
            .is_some_and(|text| probe.state(text) != State::Missing);
    let allowed = scope::authorize_file(probe, path)
        .map_err(|error| Failure::new("unsafe_file", error.to_string()))?;
    if existing && !allowed {
        return Err(Failure::new(
            "unsafe_file",
            format!(
                "Refusing to write {}: it changed while being checked; nothing was changed",
                path.display()
            ),
        )
        .hint("keep the file and its directory unchanged, then try again"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{Caller, Context, Env, change, config, keys, target};
    use super::*;
    use crate::os_service::Platform;
    use crate::update::scope_probe::Fake;
    use open_ferry_core::config::save;

    fn context(path: &Path) -> Context {
        Context {
            path: path.into(),
            env: Env {
                password: None,
                key_file: None,
            },
            key_file: None,
            yes: true,
            expect_sha256: None,
            expect_backup_sha256: None,
            ask: None,
            say: None,
            caller: Caller::Cli,
        }
    }

    fn snapshot(dir: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn refused_local_changes_and_undo_leave_the_config_and_every_sibling_untouched() {
        let dir = crate::scope::runtime_test_probe::scratch_dir();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, b"port: 1\n").unwrap();
        save::write_file(&path, b"port: 2\n").unwrap();
        let before = snapshot(dir.path());
        let data = std::fs::read(&path).unwrap();
        let ctx = context(&path);
        let backup = save::backup_path(&path);
        let fake = || {
            let mut probe = Fake::new(
                if cfg!(windows) {
                    Platform::Windows
                } else {
                    Platform::Linux
                },
                true,
            );
            for (file, _) in &before {
                probe.file(file.to_str().unwrap());
            }
            probe
        };
        let mut refused: Vec<_> = before.iter().map(|(file, _)| file.clone()).collect();
        refused.push(dir.path().into());
        for file in refused {
            let mut probe = fake();
            probe.untrust(file.to_str().unwrap());
            let error = change::write_in_file(&probe, &path, b"port: 3\n", &data).unwrap_err();
            assert_eq!(error.error, "unsafe_file", "{}", file.display());
            assert!(error.message.contains("Refusing"));
            assert_eq!(snapshot(dir.path()), before);
            let error =
                config::undo_in_file_with_probe(&probe, &ctx, &backup, &save::UndoCheck::default())
                    .unwrap_err();
            assert_eq!(error.error, "unsafe_file", "{}", file.display());
            assert_eq!(snapshot(dir.path()), before);
        }
        let mut probe = fake();
        probe.privileged = Err("token unavailable".into());
        assert!(change::write_in_file(&probe, &path, b"port: 3\n", &data).is_err());
        assert_eq!(snapshot(dir.path()), before);
        assert!(
            change::write_in_file(&fake(), &path, b"port: 3\n", &data)
                .unwrap()
                .is_none()
        );
        config::undo_in_file_with_probe(&fake(), &ctx, &backup, &save::UndoCheck::default())
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data);
        let mut probe = fake();
        probe.untrust(dir.path().to_str().unwrap());
        probe.privileged = Ok(false);
        assert!(
            change::write_in_file(&probe, &path, b"port: 4\n", &data)
                .unwrap()
                .is_none()
        );
        config::undo_in_file_with_probe(&probe, &ctx, &backup, &save::UndoCheck::default())
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data);
    }

    #[test]
    fn a_missing_sibling_is_not_created_before_the_parent_passes() {
        let dir = crate::scope::runtime_test_probe::scratch_dir();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, b"port: 1\n").unwrap();
        let mut probe = Fake::new(
            if cfg!(windows) {
                Platform::Windows
            } else {
                Platform::Linux
            },
            true,
        );
        probe
            .file(path.to_str().unwrap())
            .untrust(dir.path().to_str().unwrap());
        let before = snapshot(dir.path());
        assert!(change::write_in_file(&probe, &path, b"port: 2\n", b"port: 1\n").is_err());
        assert_eq!(snapshot(dir.path()), before);
        probe.untrusted.clear();
        assert!(
            change::write_in_file(&probe, &path, b"port: 2\n", b"port: 1\n")
                .unwrap()
                .is_none()
        );
        assert!(save::backup_path(&path).exists());
    }

    #[test]
    fn privileged_macos_local_writes_are_refused() {
        let mut probe = Fake::new(Platform::MacOs, true);
        probe.file("/work/config.yaml");
        assert!(config_write(&probe, Path::new("/work/config.yaml")).is_err());
        assert!(write(&probe, Path::new("/work/new-key")).is_err());
        probe.privileged = Ok(false);
        assert!(config_write(&probe, Path::new("/work/config.yaml")).is_ok());
        let mut container = crate::scope::tests::Fake::new(Platform::Linux, true);
        container.container = true;
        container.file("/work/config.yaml").untrust("/work");
        assert!(config_write(&container, Path::new("/work/config.yaml")).is_ok());
        assert!(write(&container, Path::new("/work/new-key")).is_ok());
    }

    #[test]
    fn key_and_value_files_are_checked_before_reading_or_creating_them() {
        let dir = crate::scope::runtime_test_probe::scratch_dir();
        let path = dir.path().join("key.txt");
        std::fs::write(&path, "plain-secret-value\n").unwrap();
        let ctx = context(&dir.path().join("config.yaml"));
        let mut probe = Fake::new(
            if cfg!(windows) {
                Platform::Windows
            } else {
                Platform::Linux
            },
            true,
        );
        probe
            .file(path.to_str().unwrap())
            .untrust(path.to_str().unwrap());
        assert_eq!(
            target::read_key_file_with_probe(&probe, &path)
                .unwrap_err()
                .error,
            "unsafe_file"
        );
        assert_eq!(
            config::read_value_file_with_probe(&probe, &ctx, &path)
                .unwrap_err()
                .error,
            "unsafe_file"
        );
        probe.untrusted.clear();
        assert_eq!(
            target::read_key_file_with_probe(&probe, &path).unwrap(),
            "plain-secret-value"
        );
        assert_eq!(
            config::read_value_file_with_probe(&probe, &ctx, &path).unwrap(),
            "plain-secret-value\n"
        );
        let new = dir.path().join("new-key.txt");
        probe.untrust(dir.path().to_str().unwrap());
        assert!(keys::write_key_file_with_probe(&probe, &new, "generated-key").is_err());
        assert!(!new.exists());
        probe.untrusted.clear();
        keys::write_key_file_with_probe(&probe, &new, "generated-key").unwrap();
        assert_eq!(std::fs::read(&new).unwrap(), b"generated-key\n");
        probe.untrust(path.to_str().unwrap());
        probe.privileged = Ok(false);
        assert!(target::read_key_file_with_probe(&probe, &path).is_ok());
        assert!(config::read_value_file_with_probe(&probe, &ctx, &path).is_ok());
        let ordinary = dir.path().join("ordinary-key.txt");
        probe.untrust(dir.path().to_str().unwrap());
        keys::write_key_file_with_probe(&probe, &ordinary, "ordinary-key").unwrap();
    }
}
