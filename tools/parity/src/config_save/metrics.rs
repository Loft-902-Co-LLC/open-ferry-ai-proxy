//! Optional, exclusive offline timing buckets. No inputs or outputs are logged.
use std::{
    cell::RefCell,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

static ENABLED: OnceLock<bool> = OnceLock::new();
static TOTALS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
thread_local! { static STACK: RefCell<Vec<(usize,Instant,Duration)>> = const { RefCell::new(Vec::new()) }; }
pub(crate) struct Timer(bool);
impl Timer {
    pub(crate) fn start(bucket: usize) -> Self {
        let enabled =
            *ENABLED.get_or_init(|| std::env::var_os("OPEN_FERRY_PARITY_METRICS").is_some());
        if enabled {
            STACK.with_borrow_mut(|stack| stack.push((bucket, Instant::now(), Duration::ZERO)));
        }
        Self(enabled)
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        if self.0 {
            STACK.with_borrow_mut(|stack| {
                let (bucket, started, children) = stack.pop().unwrap();
                let elapsed = started.elapsed();
                TOTALS[bucket].fetch_add(
                    elapsed
                        .saturating_sub(children)
                        .as_nanos()
                        .min(u64::MAX as u128) as u64,
                    Ordering::Relaxed,
                );
                if let Some(parent) = stack.last_mut() {
                    parent.2 += elapsed;
                }
            });
        }
    }
}
pub(crate) fn flush() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = std::env::var_os("OPEN_FERRY_PARITY_METRICS") {
        let values: serde_json::Map<_, _> = ["writer", "go", "counterfactual_search", "bind"]
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                (
                    name.into(),
                    serde_json::json!(TOTALS[i].load(Ordering::Relaxed) as f64 / 1e9),
                )
            })
            .collect();
        std::fs::write(path, serde_json::to_vec_pretty(&values)?)?;
    }
    Ok(())
}

pub(crate) fn reset() {
    for total in &TOTALS {
        total.store(0, Ordering::Relaxed);
    }
}
