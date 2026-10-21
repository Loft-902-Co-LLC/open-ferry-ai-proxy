//! Test-only observation of the real decoder's destructor, before its fields
//! free their allocations. The wrapper forwards every decoder operation.
use std::cell::RefCell;
use std::ops::{Deref, DerefMut};

use ruzstd::decoding::FrameDecoder;

struct Observation {
    dropping: Box<dyn FnMut()>,
    panic_after_init: bool,
}
thread_local! {
    static OBSERVATION: RefCell<Option<Observation>> = const { RefCell::new(None) };
}

pub(crate) fn start(dropping: impl FnMut() + 'static, panic_after_init: bool) {
    OBSERVATION.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(Observation {
            dropping: Box::new(dropping),
            panic_after_init,
        });
    });
}

pub(crate) fn finish() {
    OBSERVATION.with(|slot| {
        slot.borrow_mut().take();
    });
}

pub(super) fn after_init() {
    let panic = OBSERVATION.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|value| value.panic_after_init)
    });
    assert!(!panic, "injected panic with an initialized zstd decoder");
}

pub(super) struct ObservedDecoder(FrameDecoder);
impl ObservedDecoder {
    pub(super) fn new() -> Self {
        Self(FrameDecoder::new())
    }
}
impl Deref for ObservedDecoder {
    type Target = FrameDecoder;
    fn deref(&self) -> &FrameDecoder {
        &self.0
    }
}
impl DerefMut for ObservedDecoder {
    fn deref_mut(&mut self) -> &mut FrameDecoder {
        &mut self.0
    }
}
impl Drop for ObservedDecoder {
    fn drop(&mut self) {
        OBSERVATION.with(|slot| {
            if let Some(value) = slot.borrow_mut().as_mut() {
                (value.dropping)();
            }
        });
        // Rust drops self.0 immediately afterwards, while the recorded charge
        // must still cover it. No assertion/panic in this unwind-time observer.
    }
}
