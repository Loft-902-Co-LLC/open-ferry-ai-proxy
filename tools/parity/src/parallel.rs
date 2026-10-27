//! Bounded case evaluation with a single, ordered recorder.
use std::{
    collections::BTreeMap,
    panic::{self, AssertUnwindSafe},
    sync::Mutex,
    sync::mpsc,
    thread,
};

/// At most `jobs` evaluations run and at most `2 * jobs` await recording.
/// Errors belong to the recorder, so an earlier case always wins, including
/// when a later evaluation finishes (or fails) first. Dropping the task sender
/// on error stops dispatch; scoped workers finish before returning.
pub(crate) fn for_each<T: Send, E>(
    count: usize,
    jobs: usize,
    evaluate: impl Fn(usize) -> T + Sync,
    mut record: impl FnMut(usize, T) -> Result<(), E>,
) -> Result<(), E> {
    let jobs = jobs.max(1).min(count);
    if jobs <= 1 {
        return (0..count).try_for_each(|index| record(index, evaluate(index)));
    }
    let (tasks, work) = mpsc::channel();
    let work = Mutex::new(work);
    let (completed, results) = mpsc::channel();
    thread::scope(|scope| {
        for _ in 0..jobs {
            let (work, evaluate, completed) = (&work, &evaluate, completed.clone());
            scope.spawn(move || {
                loop {
                    let Ok(index) = work.lock().unwrap().recv() else {
                        break;
                    };
                    if completed
                        .send((
                            index,
                            panic::catch_unwind(AssertUnwindSafe(|| evaluate(index))),
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(completed);
        let recorded = (|| {
            let window = jobs.saturating_mul(2).min(count);
            for index in 0..window {
                tasks.send(index).unwrap();
            }
            let mut next = 0;
            let mut pending = BTreeMap::new();
            for (index, result) in results {
                pending.insert(index, result);
                while let Some(result) = pending.remove(&next) {
                    let result = result.unwrap_or_else(|payload| panic::resume_unwind(payload));
                    record(next, result)?;
                    if let Some(index) = next.checked_add(window).filter(|&i| i < count) {
                        tasks.send(index).unwrap();
                    }
                    next += 1;
                    if next == count {
                        return Ok(());
                    }
                }
            }
            unreachable!("case worker panicked")
        })();
        drop(tasks);
        recorded
    })
}
