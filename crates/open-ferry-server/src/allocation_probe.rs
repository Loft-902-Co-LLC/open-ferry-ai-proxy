use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
#[derive(Clone, Copy)]
struct Allocation { pointer: usize, bytes: usize }
const EMPTY: Allocation = Allocation { pointer: 0, bytes: 0 };
struct State { allocations: [Allocation; 1024], live: usize, peak: usize, largest: usize, overflow: bool }
#[derive(Debug)]
pub(crate) struct Measurements { pub peak: usize, pub largest: usize, pub overflow: bool }
thread_local! { static ACTIVE: RefCell<Option<State>> = const { RefCell::new(None) }; }
struct Probe;
#[global_allocator]
static ALLOCATOR: Probe = Probe;
fn inspect(f: impl FnOnce(&mut State)) {
    let _ = ACTIVE.try_with(|active| { if let Some(state)=active.borrow_mut().as_mut() { f(state); } });
}
fn record(pointer: *mut u8, bytes: usize) {
    if pointer.is_null() { return; }
    inspect(|state| {
        if let Some(slot)=state.allocations.iter_mut().find(|slot| slot.pointer==0) {
            *slot=Allocation { pointer:pointer as usize, bytes };
            state.live+=bytes;
            state.peak=state.peak.max(state.live);
            state.largest=state.largest.max(bytes);
        } else { state.overflow=true; }
    });
}
fn forget(pointer: *mut u8) {
    inspect(|state| { if let Some(slot)=state.allocations.iter_mut().find(|slot| slot.pointer==pointer as usize) {
        state.live-=slot.bytes; *slot=EMPTY;
    } });
}
// SAFETY: all requests are forwarded unchanged to System. Tracking uses only
// thread-local fixed storage, without allocating or taking a lock. Realloc
// conservatively counts overlap of the old and new requested layouts.
unsafe impl GlobalAlloc for Probe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer=unsafe { System.alloc(layout) }; record(pointer,layout.size()); pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer=unsafe { System.alloc_zeroed(layout) }; record(pointer,layout.size()); pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        forget(pointer); unsafe { System.dealloc(pointer,layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, bytes: usize) -> *mut u8 {
        inspect(|state| { state.peak=state.peak.max(state.live+bytes); });
        let replacement=unsafe { System.realloc(pointer,layout,bytes) };
        if !replacement.is_null() { forget(pointer); record(replacement,bytes); }
        replacement
    }
}
pub(crate) fn start() {
    ACTIVE.with(|active| *active.borrow_mut()=Some(State { allocations:[EMPTY;1024],live:0,peak:0,largest:0,overflow:false }));
}
pub(crate) fn finish_stats() -> Measurements {
    ACTIVE.with(|active| { let state=active.borrow_mut().take().unwrap(); Measurements { peak:state.peak,largest:state.largest,overflow:state.overflow } })
}
pub(crate) fn finish() -> usize { finish_stats().largest }
