//! Tests of the usage statistics, one module per upstream test file, and
//! [`support`] for what they share.
//!
//! Upstream's internal/runtime/executor/helps/responses_usage_helpers_test.go
//! isn't here: its `EnsureResponsesUsageDetails` belongs to the Codex
//! provider, and is ported with its tests in open-ferry-providers'
//! codex/usage.rs.

mod accounting;
mod error_events;
mod manager;
mod plugin;
mod queue;
mod response_model;
mod response_model_multiprovider;
mod responses_ttft_helpers;
mod stream_response_model_observer;
mod support;
mod usage_helpers;
