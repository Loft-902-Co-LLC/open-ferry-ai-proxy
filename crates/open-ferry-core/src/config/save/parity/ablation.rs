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
    EffectiveBlockScalarComment,
    BlockScalarCommentIndentation,
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

/// Rejected intermediate/final bytes, observed without permitting a bad write.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Rejected {
    pub stage: &'static str,
    pub bytes: Vec<u8>,
}
/// Exact comment write, excluding its terminating YAML line break.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WrittenComment {
    pub offset: usize,
    pub bytes: Vec<u8>,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct Emission {
    pub bytes: Vec<u8>,
    pub comments: Vec<WrittenComment>,
}
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Trace {
    pub rejected: Vec<Rejected>,
    pub emissions: Vec<Emission>,
}
thread_local! {
    static TRACE: std::cell::RefCell<Option<Trace>> = const { std::cell::RefCell::new(None) };
}
pub(crate) fn tracing() -> bool {
    TRACE.with_borrow(Option::is_some)
}
pub(crate) fn record_rejected(stage: &'static str, bytes: &[u8]) {
    TRACE.with_borrow_mut(|state| {
        if let Some(t) = state {
            t.rejected.push(Rejected {
                stage,
                bytes: bytes.to_vec(),
            });
        }
    });
}
pub(crate) fn record_emission(bytes: &[u8], comments: &[WrittenComment]) {
    TRACE.with_borrow_mut(|state| {
        if let Some(t) = state {
            t.emissions.push(Emission {
                bytes: bytes.to_vec(),
                comments: comments.to_vec(),
            });
        }
    });
}
/// Observe one diagnostic call; nesting and unwinding restore the outer trace.
pub fn with_trace<T>(call: impl FnOnce() -> T) -> (T, Trace) {
    struct Restore(Option<Trace>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TRACE.with_borrow_mut(|s| *s = self.0.take());
        }
    }
    let _restore = Restore(TRACE.with(|s| s.replace(Some(Trace::default()))));
    let result = call();
    let trace = TRACE.with_borrow_mut(|s| s.take().unwrap_or_default());
    (result, trace)
}

/// Bind the recorded writes directly to this complete emission. No string
/// search or inferred recipient can repair an offset or an absent write.
pub fn bind_comments(emission: &Emission, output: &[u8]) -> Option<Vec<WrittenComment>> {
    if emission.bytes != output {
        return None;
    }
    let mut end = 0;
    for c in &emission.comments {
        let next = c.offset.checked_add(c.bytes.len())?;
        if c.offset < end
            || c.bytes.is_empty()
            || c.bytes[0] != b'#'
            || output.get(c.offset..next)? != c.bytes
        {
            return None;
        }
        end = next;
    }
    Some(emission.comments.clone())
}

/// Observe the actual writer postprocessing of the same emission. Keep its
/// comment offsets current as indentation is removed, so proof consumers bind
/// the recorded offsets directly to final bytes (never to a guessed rewrite).
pub(crate) fn record_normalization(input: &[u8], output: &[u8]) {
    if input == output {
        return;
    }
    TRACE.with_borrow_mut(|state| {
        let Some(trace) = state else {
            return;
        };
        let Some(emission) = trace.emissions.iter().rev().find(|e| e.bytes == input) else {
            return;
        };
        let Some(mut comments) = bind_comments(emission, input) else {
            return;
        };
        let old_lines: Vec<_> = input.split_inclusive(|b| *b == b'\n').collect();
        let new_lines: Vec<_> = output.split_inclusive(|b| *b == b'\n').collect();
        if old_lines.len() != new_lines.len() {
            return;
        }
        let (mut old_at, mut new_at) = (0, 0);
        for (old, new) in old_lines.into_iter().zip(new_lines) {
            let Some(removed) = old.len().checked_sub(new.len()) else {
                return;
            };
            if old.get(removed..) != Some(new)
                || old[..removed].iter().any(|b| !matches!(b, b' ' | b'\t'))
            {
                return;
            }
            for (mapped, original) in comments.iter_mut().zip(&emission.comments) {
                if original.offset >= old_at && original.offset < old_at + old.len() {
                    let Some(relative) = original.offset.checked_sub(old_at + removed) else {
                        return;
                    };
                    mapped.offset = new_at + relative;
                }
            }
            old_at += old.len();
            new_at += new.len();
        }
        let normalized = Emission {
            bytes: output.to_vec(),
            comments,
        };
        if bind_comments(&normalized, output).is_some() {
            trace.emissions.push(normalized);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Protection; 6] = [
        Protection::EmptyMappingPendingComment,
        Protection::CanonicalVersionComment,
        Protection::DroppedSingleKeyComment,
        Protection::InlineEmptyNullComment,
        Protection::EffectiveBlockScalarComment,
        Protection::BlockScalarCommentIndentation,
    ];

    #[test]
    fn normalized_trace_offsets_name_the_actual_final_bytes() {
        let doc = super::super::super::unmarshal(
            b"plugins:\n  configs:\n    demo:\n      # head\n      x: 1 # inline\n",
        )
        .unwrap();
        let ((raw, normalized), trace) = with_trace(|| {
            let raw = super::super::super::marshal_config(&doc).unwrap();
            let normalized = super::super::super::normalize_comment_indentation(&raw);
            (raw, normalized)
        });
        assert_ne!(raw, normalized);
        let emission = trace
            .emissions
            .iter()
            .rev()
            .find(|e| e.bytes == normalized)
            .unwrap();
        let comments = bind_comments(emission, &normalized).unwrap();
        assert_eq!(comments.len(), 2);
        for c in comments {
            assert_eq!(&normalized[c.offset..c.offset + c.bytes.len()], c.bytes);
        }
        let raw_emission = trace.emissions.iter().find(|e| e.bytes == raw).unwrap();
        assert!(bind_comments(raw_emission, &normalized).is_none());
    }

    #[test]
    fn nested_trace_scopes_restore_after_unwinding() {
        let (_, outer) = with_trace(|| {
            record_rejected("outer", b"first");
            let (_, inner) = with_trace(|| record_rejected("inner", b"second"));
            assert_eq!(inner.rejected.len(), 1);
            assert!(
                std::panic::catch_unwind(|| with_trace(|| panic!("trace restoration"))).is_err()
            );
            record_rejected("outer", b"last");
        });
        assert_eq!(outer.rejected.len(), 2);
        assert!(!tracing());
    }

    #[test]
    fn write_trace_is_byte_bound_and_nonoverlapping() {
        let bytes = b"x: |\n# own\n  value\n";
        let emission = Emission {
            bytes: bytes.to_vec(),
            comments: vec![WrittenComment {
                offset: 5,
                bytes: b"# own".to_vec(),
            }],
        };
        assert_eq!(bytes[5], b'#');
        assert!(bind_comments(&emission, bytes).is_some());
        let mut wrong = emission.clone();
        wrong.comments[0].offset += 1;
        assert!(bind_comments(&wrong, bytes).is_none());
        wrong = emission.clone();
        wrong.comments[0].bytes = b"# absent".to_vec();
        assert!(bind_comments(&wrong, bytes).is_none());
        wrong = emission.clone();
        wrong.comments.push(wrong.comments[0].clone());
        assert!(bind_comments(&wrong, bytes).is_none());
        assert!(bind_comments(&emission, b"x: |\n# other\n  value\n").is_none());
    }

    #[test]
    fn emitter_trace_records_only_comment_writes_with_real_offsets() {
        let doc =
            super::super::super::unmarshal(b"x: 'data # own' # own\ny: |\n  data # own\n").unwrap();
        let (bytes, trace) = with_trace(|| super::super::super::marshal_config(&doc).unwrap());
        let emitted = trace.emissions.iter().find(|e| e.bytes == bytes).unwrap();
        let bound = bind_comments(emitted, &bytes).unwrap();
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].bytes, b"# own");
    }

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
