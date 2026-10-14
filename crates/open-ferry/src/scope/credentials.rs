//! Credential leaves need their own trust check, even in a trusted directory.

use super::*;
use open_ferry_core::auth::file_guard::FileGuard;

/// Checks an existing credential file, or the directory of a new save target.
pub(crate) fn authorize_credential_file(probe: &dyn Probe, path: &Path) -> Result<(), Refusal> {
    let refuse = |reason| Refusal {
        reason: format!("Refusing credential file {}: {reason}", path.display()),
        remedy: Some(trust_remedy(probe.platform(), "the credential file")),
    };
    let Some(text) = privileged_path(probe, path, &refuse)? else {
        return Ok(());
    };
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(refuse("it is a link".to_owned()));
    }
    match probe.state(text) {
        State::Missing => {
            let parent = path
                .parent()
                .ok_or_else(|| refuse("it has no parent".to_owned()))?;
            if !authorize_dir(probe, parent)? {
                return Err(refuse("its directory doesn't exist".to_owned()));
            }
            Ok(())
        }
        State::File => match authorize_file(probe, path)? {
            true => Ok(()),
            false => Err(refuse("it changed while being checked".to_owned())),
        },
        _ => Err(refuse("it isn't an accessible real file".to_owned())),
    }
}

/// Uses the same full auth-directory path as the service under privilege.
/// Call after authorizing the original path, so dot components still refuse.
pub(crate) fn absolute_auth_dir(probe: &dyn Probe, dir: std::path::PathBuf) -> std::path::PathBuf {
    if matches!(probe.privileged(), Ok(false)) || probe.in_container() {
        return dir;
    }
    match std::path::absolute(&dir) {
        Ok(absolute) => open_ferry_core::auth::path::clean(&absolute),
        Err(_) => dir,
    }
}

/// Shares the runtime policy and warning history among credential I/O sites.
pub(crate) fn credential_file_guard(probe: RuntimeProbe) -> Option<FileGuard> {
    match probe.privileged() {
        Ok(false) => return None,
        Ok(true) if probe.in_container() => return None,
        _ => {}
    }
    Some(FileGuard::new(move |path| {
        authorize_credential_file(probe.as_ref(), path).map_err(|error| error.to_string())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::scope_probe::Fake;

    struct VanishingFile {
        fake: super::super::runtime_test_probe::RuntimeFake,
        path: String,
        lookups: std::sync::atomic::AtomicUsize,
    }

    impl Probe for VanishingFile {
        fn platform(&self) -> Platform {
            self.fake.platform()
        }
        fn privileged(&self) -> Result<bool, String> {
            self.fake.privileged()
        }
        fn account_dir(&self) -> Result<String, String> {
            self.fake.account_dir()
        }
        fn program_data(&self) -> Result<String, String> {
            self.fake.program_data()
        }
        fn state(&self, path: &str) -> State {
            if self.platform().clean(path) == self.path {
                if self
                    .lookups
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    == 0
                {
                    State::File
                } else {
                    State::Missing
                }
            } else {
                self.fake.state(path)
            }
        }
        fn entries(&self, dir: &str, limit: usize) -> io::Result<Vec<String>> {
            self.fake.entries(dir, limit)
        }
        fn trusted_dir(&self, path: &str) -> Result<(), String> {
            self.fake.trusted_dir(path)
        }
        fn trusted_file(&self, path: &str) -> Result<(), String> {
            self.fake.trusted_file(path)
        }
        fn readable_file(&self, path: &str) -> io::Result<()> {
            self.fake.readable_file(path)
        }
        fn receipt(&self, path: &str) -> Result<Option<Receipt>, ReceiptError> {
            self.fake.receipt(path)
        }
        fn canonical(&self, path: &str) -> io::Result<String> {
            self.fake.canonical(path)
        }
        fn create_machine_dir(&mut self, _: &str) -> io::Result<()> {
            panic!("never provision")
        }
    }

    #[test]
    fn a_credential_that_vanishes_during_authorization_never_reaches_the_reader() {
        use open_ferry_core::auth::{Auth, AuthStore, FileStore};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = super::super::runtime_test_probe::scratch_dir();
        let root = open_ferry_core::auth::path::clean(dir.path());
        let path = root.join("vanishing.json");
        fs::write(&path, br#"{"type":"codex"}"#).unwrap();
        let platform = Platform::HOST.unwrap();
        let mut fake = Fake::new(platform, true);
        fake.file(path.to_str().unwrap());
        let probe = Arc::new(VanishingFile {
            fake: super::super::runtime_test_probe::RuntimeFake::new(fake),
            path: platform.clean(path.to_str().unwrap()),
            lookups: AtomicUsize::new(0),
        });
        let refusal = authorize_credential_file(probe.as_ref(), &path).unwrap_err();
        assert!(refusal.reason.contains("it changed while being checked"));
        probe.lookups.store(0, Ordering::SeqCst);
        let runtime: RuntimeProbe = probe.clone();
        let store = FileStore::new(&root).with_file_guard(credential_file_guard(runtime));
        let auth = Auth {
            file_name: "vanishing.json".into(),
            ..Auth::default()
        };
        let reads = std::cell::Cell::new(0);
        assert!(
            store
                .read_existing_metadata(&auth, |_| {
                    reads.set(reads.get() + 1);
                    Ok(b"{}".to_vec())
                })
                .is_err()
        );
        assert_eq!(reads.get(), 0);
        probe.lookups.store(0, Ordering::SeqCst);
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn auth_directory_paths_are_made_full_only_under_privilege() {
        let relative = std::path::PathBuf::from("auth");
        let mut fake = Fake::new(Platform::HOST.unwrap(), true);
        assert_eq!(
            absolute_auth_dir(&fake, relative.clone()),
            open_ferry_core::auth::path::clean(&std::path::absolute(&relative).unwrap())
        );
        fake.privileged = Ok(false);
        assert_eq!(absolute_auth_dir(&fake, relative.clone()), relative);
    }

    #[test]
    fn credential_leaves_and_new_targets_need_their_own_trust() {
        let mut fake = Fake::new(Platform::Linux, true);
        fake.file("/work/auth/trusted.json")
            .file("/work/auth/unsafe.json")
            .untrust("/work/auth/unsafe.json");
        assert!(authorize_auth_dir(&fake, Path::new("/work/auth")).is_ok());
        assert!(authorize_credential_file(&fake, Path::new("/work/auth/trusted.json")).is_ok());
        assert!(authorize_credential_file(&fake, Path::new("/work/auth/unsafe.json")).is_err());
        assert!(authorize_credential_file(&fake, Path::new("/work/auth/new.json")).is_ok());
        fake.untrust("/work/auth");
        assert!(authorize_credential_file(&fake, Path::new("/work/auth/new.json")).is_err());
        assert!(authorize_credential_file(&fake, Path::new("/absent/new.json")).is_err());
    }

    #[test]
    fn unprivileged_credentials_are_unchanged_and_privilege_errors_fail_closed() {
        let mut fake = Fake::new(Platform::Linux, false);
        fake.file("/work/auth/unsafe.json").untrust("/work");
        assert!(authorize_credential_file(&fake, Path::new("/work/auth/unsafe.json")).is_ok());
        let probe: RuntimeProbe =
            Arc::new(super::super::runtime_test_probe::RuntimeFake::new(fake));
        assert!(credential_file_guard(probe).is_none());
        let mut fake = Fake::new(Platform::Linux, false);
        fake.privileged = Err("token unavailable".to_owned());
        assert!(authorize_credential_file(&fake, Path::new("/work/auth/unsafe.json")).is_err());
        let probe: RuntimeProbe =
            Arc::new(super::super::runtime_test_probe::RuntimeFake::new(fake));
        assert!(credential_file_guard(probe).is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_in_a_trusted_directory_is_not_loaded_watched_or_saved() {
        use open_ferry_core::auth::FileStore;
        use open_ferry_core::auth::synthesizer::{SynthesisContext, file::synthesize_file_auths};
        use open_ferry_core::config::{Config, ConfigWatcher, WatchEvent};
        let dir = super::super::runtime_test_probe::scratch_dir();
        let scratch_path =
            std::path::PathBuf::from(Platform::Linux.clean(dir.path().to_str().unwrap()));
        let auth_dir = scratch_path.join("auth");
        fs::create_dir(&auth_dir).unwrap();
        let target = scratch_path.join("target.json");
        fs::write(&target, br#"{"type":"codex"}"#).unwrap();
        let link = auth_dir.join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let config_path = scratch_path.join("config.yaml");
        fs::write(&config_path, "port: 9292\n").unwrap();
        let mut fake = Fake::new(Platform::Linux, true);
        fake.file(link.to_str().unwrap())
            .untrust(target.to_str().unwrap());
        assert!(authorize_auth_dir(&fake, &auth_dir).is_ok());
        assert!(authorize_credential_file(&fake, &link).is_err());
        let probe: RuntimeProbe =
            Arc::new(super::super::runtime_test_probe::RuntimeFake::new(fake));
        let guard = credential_file_guard(probe);
        let mut ctx = SynthesisContext::new(&auth_dir, chrono::Utc::now());
        ctx.file_guard = guard.clone();
        assert!(synthesize_file_auths(&ctx).is_empty());
        let store = FileStore::new(&auth_dir).with_file_guard(guard.clone());
        assert!(store.write_file("link.json", b"{}").is_err());
        assert_eq!(fs::read(&target).unwrap(), br#"{"type":"codex"}"#);
        let mut config = Config::default();
        config.auth_dir = auth_dir.to_string_lossy().into_owned();
        let (watcher, mut events) =
            ConfigWatcher::start_with_guards(&config_path, &config, None, guard).unwrap();
        watcher.force_reload_config(7);
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        // No AuthAdded precedes the config reload barrier.
        assert!(matches!(event, WatchEvent::ConfigChanged(..)), "{event:?}");
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap();
        assert!(matches!(event, Some(WatchEvent::Reloaded(7))));
    }
}
