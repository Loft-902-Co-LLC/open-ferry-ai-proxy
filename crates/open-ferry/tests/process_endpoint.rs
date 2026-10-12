//! Not upstream's: the supervisor crate's `process_endpoint` tests, run against the
//! real `open-ferry` worker instead of its fake. The file is included by path
//! with the harness (`under_test`), which picks the worker from this crate's
//! own binaries; a test the real worker can't pass is skipped by capability
//! (`lacks`), not by name. One binary per file keeps the harness's gate (how
//! many supervisors run at once) per file, as in the supervisor crate.
#![cfg(feature = "blue-green-test")]

#[path = "../../open-ferry-supervisor/tests/process_endpoint.rs"]
mod rows;
