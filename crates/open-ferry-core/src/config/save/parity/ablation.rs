//! Call-scoped writer diagnostics for offline parity only.
//!
//! This entire module and every call to it are absent without `parity`.
//! Disabling these protections is never a supported product operation.

use std::cell::Cell;

/// Independently ablatable comment protections. Scalar-value protections
/// deliberately remain enabled in every diagnostic run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Protection {
    EmptyMappingPendingComment,
    CanonicalVersionComment,
    DroppedSingleKeyComment,
    InlineEmptyNullComment,
}

thread_local! {
    static DISABLED: Cell<u8> = const { Cell::new(0) };
}

pub(crate) fn enabled(protection: Protection) -> bool {
    DISABLED.with(|state| state.get() & (1 << protection as u8) == 0)
}

/// Run one synchronous parity call with the given protections disabled.
/// Nested scopes inherit the outer scope; unwinding restores it too.
/// The scope does not propagate to other threads or asynchronous tasks.
pub fn with_disabled<T>(protections: &[Protection], call: impl FnOnce() -> T) -> T {
    struct Restore(u8);
    impl Drop for Restore {
        fn drop(&mut self) {
            DISABLED.with(|state| state.set(self.0));
        }
    }
    let mask = protections.iter().fold(0, |mask, p| mask | (1 << *p as u8));
    let _restore = Restore(DISABLED.with(|state| state.replace(state.get() | mask)));
    call()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Protection; 4] = [
        Protection::EmptyMappingPendingComment,
        Protection::CanonicalVersionComment,
        Protection::DroppedSingleKeyComment,
        Protection::InlineEmptyNullComment,
    ];

    #[test]
    fn switches_are_independent_and_restore_after_each_call() {
        for protection in ALL {
            assert!(ALL.into_iter().all(enabled));
            with_disabled(&[protection], || {
                for other in ALL {
                    assert_eq!(enabled(other), other != protection);
                }
            });
        }
        assert!(ALL.into_iter().all(enabled));
    }

    #[test]
    fn nested_scopes_restore_after_unwinding() {
        with_disabled(&ALL[..1], || {
            let caught = std::panic::catch_unwind(|| {
                with_disabled(&ALL[1..], || {
                    assert!(ALL.into_iter().all(|p| !enabled(p)));
                    panic!("test scope restoration");
                });
            });
            assert!(caught.is_err());
            assert!(!enabled(ALL[0]));
            assert!(ALL[1..].iter().copied().all(enabled));
        });
        assert!(ALL.into_iter().all(enabled));
    }

    #[test]
    fn scope_does_not_disable_another_threads_protections() {
        with_disabled(&ALL, || {
            assert!(ALL.into_iter().all(|p| !enabled(p)));
            std::thread::spawn(|| assert!(ALL.into_iter().all(enabled)))
                .join()
                .unwrap();
        });
        assert!(ALL.into_iter().all(enabled));
    }
}
