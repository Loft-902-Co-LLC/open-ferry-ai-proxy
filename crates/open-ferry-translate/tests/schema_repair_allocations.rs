//! Keep this allocator regression in its own test binary to isolate accounting.

use open_ferry_translate::gemini::claude::convert_claude_request_to_gemini;
use open_ferry_translate::models::ModelCatalog;
use serde_json::{Value, json};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let current = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(current, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let pointer = unsafe { System.realloc(pointer, layout, size) };
        if !pointer.is_null() {
            if size >= layout.size() {
                let added = size - layout.size();
                let current = CURRENT.fetch_add(added, Ordering::Relaxed) + added;
                PEAK.fetch_max(current, Ordering::Relaxed);
            } else {
                CURRENT.fetch_sub(layout.size() - size, Ordering::Relaxed);
            }
        }
        pointer
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn repair_peak_allocation_stays_within_four_requests() {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            let catalog = ModelCatalog::embedded();
            let original = json!({"tools":[{"name":"T","input_schema":{
                "type":"object","properties":{"p":{"type":"string"}},"additionalProperties":false}}]});
            let mut schema = json!({"type":"string","description":"x".repeat(1 << 20)});
            for _ in 0..60 {
                schema = json!({"type":"array","items":schema});
            }
            let sent = json!({"tools":[{"functionDeclarations":[{"name":"T","parametersJsonSchema":schema}]}]});
            let sent_bytes = serde_json::to_vec(&sent).unwrap().len();
            for calls in [1, 16] {
                let parts: Vec<_> = (0..calls).map(|_| json!({"functionCall":{
                    "name":"T","args":{"p":"x","_":true}}})).collect();
                let response = json!({"candidates":[{"content":{"parts":parts},"finishReason":"STOP"}]});
                let base = CURRENT.load(Ordering::Relaxed);
                PEAK.store(base, Ordering::Relaxed);
                let out = open_ferry_translate::gemini::claude::convert_gemini_response_to_claude_non_stream_with_translated(
                    &original, &sent, &response);
                let extra = PEAK.load(Ordering::Relaxed).saturating_sub(base);
                eprintln!("reclean_calls={calls} request_bytes={sent_bytes} peak_extra_bytes={extra}");
                assert!(extra <= 4 * sent_bytes, "{extra} extra bytes for {sent_bytes} schema bytes");
                assert_eq!(out["content"].as_array().unwrap().len(), calls);
                for block in out["content"].as_array().unwrap() {
                    assert_eq!(block["input"], json!({"p":"x","_":true}));
                }
            }
            let mut schema = json!({"type":"string","description":"x".repeat(1 << 20)});
            for _ in 0..120 {
                schema = json!({"type":"array","items":schema});
            }
            let request = json!({"messages":[{"role":"user","content":"hi"}],
            "tools":[{"name":"T","input_schema":schema}]});
            let bytes = serde_json::to_vec(&request).unwrap();
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            let base = CURRENT.load(Ordering::Relaxed);
            PEAK.store(base, Ordering::Relaxed);
            let (out, error) =
                convert_claude_request_to_gemini("gemini-2.5-pro", &request, false, catalog);
            let extra = PEAK.load(Ordering::Relaxed).saturating_sub(base);
            eprintln!("request_bytes={} peak_extra_bytes={extra}", bytes.len());
            assert!(error.is_none());
            assert!(
                extra <= 4 * bytes.len(),
                "{extra} extra bytes for {} request bytes",
                bytes.len()
            );
            assert!(!serde_json::to_vec(&out).unwrap().is_empty());
            drop(out);
            drop(request);
        })
        .unwrap()
        .join()
        .unwrap();
}
