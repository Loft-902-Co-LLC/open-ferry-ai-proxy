use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
thread_local! { static LARGEST: Cell<Option<usize>> = const { Cell::new(None) }; }
struct Probe;
#[global_allocator]
static ALLOCATOR: Probe = Probe;
fn record(size: usize) {
    let _ = LARGEST.try_with(|largest| {
        if let Some(previous) = largest.get() {
            largest.set(Some(previous.max(size)));
        }
    });
}
// SAFETY: every allocation and deallocation is forwarded unchanged to System.
unsafe impl GlobalAlloc for Probe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
}
pub(crate) fn start() {
    LARGEST.with(|largest| largest.set(Some(0)));
}
pub(crate) fn finish() -> usize {
    LARGEST.with(|largest| largest.replace(None).unwrap())
}
