// Ported from CLIProxyAPI sdk/cliproxy/auth/codex_siwc_bind.go
// (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Bind admission fences registration mutation through the durable file write.
//! The existing reload barrier is acquired without waiting while a file lock
//! is held. Busy callers release their file lock and retry, avoiding inversion
//! with ordinary persistence. No manager state mutex is held during file I/O.

use super::{Manager, ReloadGuard, lock};
use crate::auth::{Auth, AuthKind, AuthSource};
use std::io;

impl Manager {
    /// Runs a short synchronous SIWC disk commit against its start-time
    /// registration. `None` means the mutation barrier is busy: release all
    /// store/file locks before retrying. `commit` must not call the manager.
    #[doc(hidden)]
    pub fn try_commit_siwc_bind<T>(
        &self,
        expected: &Auth,
        commit: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        let barrier = &self.shared.load_gate;
        let gate = {
            let mut state = lock(&barrier.state);
            if state.0 != 0 || state.1 || state.2 != 0 {
                return Ok(None);
            }
            state.1 = true;
            ReloadGuard {
                barrier: barrier.clone(),
                reload: true,
            }
        };
        let current = self
            .get(&expected.id)
            .ok_or_else(|| io::Error::other("SIWC bind registration changed"))?;
        if !current.provider.trim().eq_ignore_ascii_case("codex")
            || current.auth_kind() != Some(AuthKind::OAuth)
            || current.auth_source_kind() != Some(AuthSource::File)
            || current
                .attribute("path")
                .unwrap_or_default()
                .trim()
                .is_empty()
            || ["access_token", "refresh_token"].iter().any(|key| {
                current
                    .metadata_str(key)
                    .is_none_or(|value| value.trim().is_empty())
            })
            || expected.registration_epoch == 0
            || current.registration_epoch != expected.registration_epoch
            || current.provider != expected.provider
            || current.file_name != expected.file_name
            || current.attribute("path") != expected.attribute("path")
            || current.auth_source_kind() != expected.auth_source_kind()
            || current.auth_kind() != expected.auth_kind()
        {
            return Err(io::Error::other("SIWC bind registration changed"));
        }
        crate::auth::file_store::check_bind_metadata(&expected.metadata, &current.metadata)?;
        let result = commit().map(Some);
        drop(gate);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::Settings;
    use crate::registry::ModelRegistry;
    use std::sync::Arc;

    fn setup() -> (Manager, Arc<Auth>) {
        let manager = Manager::new(Settings::default(), Arc::new(ModelRegistry::new()), None);
        let expected = manager
            .register_unsaved(Auth {
                id: "dummy.json".into(),
                provider: "codex".into(),
                file_name: "dummy.json".into(),
                attributes: [("path".to_owned(), "dummy.json".to_owned())].into(),
                metadata: serde_json::json!({"type":"codex", "access_token":"dummy-access", "refresh_token":"dummy-refresh"}).as_object().unwrap().clone(),
                ..Auth::default()
            })
            .unwrap();
        (manager, expected)
    }

    #[test]
    fn registration_replacement_and_busy_admission_never_commit() {
        let (manager, expected) = setup();
        let shared = manager.mutation_gate();
        assert!(
            manager
                .try_commit_siwc_bind(&expected, || Ok(()))
                .unwrap()
                .is_none()
        );
        drop(shared);
        manager.remove(&expected.id);
        manager.register_unsaved(Auth::clone(&expected)).unwrap();
        assert!(
            manager
                .try_commit_siwc_bind(&expected, || -> io::Result<()> {
                    panic!("stale registration committed")
                })
                .is_err()
        );
    }

    #[test]
    fn registration_mutation_waits_for_a_durable_commit() {
        let (manager, expected) = setup();
        let (started, waiting) = std::sync::mpsc::channel();
        let (finished, done) = std::sync::mpsc::channel();
        let mut task = None;
        manager
            .try_commit_siwc_bind(&expected, || {
                let changing = manager.clone();
                let id = expected.id.clone();
                task = Some(std::thread::spawn(move || {
                    started.send(()).unwrap();
                    changing.remove(&id);
                    finished.send(()).unwrap();
                }));
                waiting.recv().unwrap();
                assert!(
                    done.recv_timeout(std::time::Duration::from_millis(50))
                        .is_err()
                );
                Ok(())
            })
            .unwrap()
            .unwrap();
        done.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        task.unwrap().join().unwrap();
        assert!(manager.get(&expected.id).is_none());
    }
}
