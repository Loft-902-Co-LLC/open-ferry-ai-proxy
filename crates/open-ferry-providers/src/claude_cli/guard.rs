//! The application's check before Claude Code runs.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::Entry;

type Check = dyn Fn(&Entry, &Path, &Path) -> Result<(), String> + Send + Sync;

/// A spawn check supplied by the application, with refusals logged once
/// for each entry and reason. Returned errors never contain the reason.
#[derive(Clone)]
pub struct SpawnGuard {
    check: Arc<Check>,
    logged: Arc<Mutex<HashSet<(String, String)>>>,
}

impl fmt::Debug for SpawnGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpawnGuard").finish_non_exhaustive()
    }
}

impl SpawnGuard {
    /// Checks the resolved program, account directory and working paths.
    /// Called before creating directories and again before spawning.
    pub fn new(
        check: impl Fn(&Entry, &Path, &Path) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            check: Arc::new(check),
            logged: Arc::default(),
        }
    }

    pub(crate) fn authorize(
        &self,
        entry: &Entry,
        program: &Path,
        root: &Path,
    ) -> Result<(), String> {
        if let Err(reason) = (self.check)(entry, program, root) {
            let mut logged = self
                .logged
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if logged.insert((entry.name.clone(), reason.clone())) {
                tracing::warn!(entry = %entry.name, %reason, "claude-cli execution refused");
            }
            return Err(
                "claude-cli execution refused; see the service log for the remedy".to_owned(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_are_logged_once_per_entry_and_reason_and_checks_stay_per_instance() {
        let entry = Entry::from_auth(&open_ferry_core::auth::Auth::default());
        let guard = SpawnGuard::new(|_, _, _| Err("private reason".into()));
        for _ in 0..3 {
            let error = guard
                .authorize(&entry, Path::new("program"), Path::new("root"))
                .unwrap_err();
            assert!(!error.contains("private reason"));
        }
        assert_eq!(guard.logged.lock().unwrap().len(), 1);
        let allowed = SpawnGuard::new(|_, _, _| Ok(()));
        assert!(
            allowed
                .authorize(&entry, Path::new("program"), Path::new("root"))
                .is_ok()
        );
        assert!(allowed.logged.lock().unwrap().is_empty());
    }
}
