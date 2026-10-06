// Ported from CLIProxyAPI sdk/cliproxy/auth/*_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the manager as a whole, with a fake executor, registry and
//! store. Time is a [`TestClock`]: a fixed base, moved by hand or by paused
//! Tokio time.
//!
//! Deviations from upstream:
//! - Upstream's tests share a global model registry; each test here has its
//!   own [`FakeModels`].

mod api_key_model_alias;
mod auto_refresh_issue6199;
mod auto_refresh_loop;
mod catalog_credential_quota_regression;
mod classification;
mod claude_ratelimit_cooldown;
mod codex_forcemap_ws_forward;
mod codex_model_not_found_cooldown;
mod conductor_alias_cooldown;
mod conductor_availability;
mod conductor_claude_cancellation;
mod conductor_cloudflare_520;
mod conductor_compact_cooldown;
mod conductor_cooldown_monotonic;
mod conductor_cooling_precedence;
mod conductor_execution_error_priority;
mod conductor_executor_replace;
mod conductor_fast_error;
mod conductor_force_mapping;
mod conductor_load_persistence;
mod conductor_oauth_alias_nofork;
mod conductor_oauth_alias_suspension;
mod conductor_oauth_request_scoped_errors;
mod conductor_overrides;
mod conductor_persist_failure_logging;
mod conductor_quota_clock;
mod conductor_recent_requests;
mod conductor_refresh_disabled;
mod conductor_refresh_executor_key;
mod conductor_remove;
mod conductor_request_scoped_errors;
mod conductor_retry_round;
mod conductor_scheduler_cooldown_rebuild;
mod conductor_scheduler_refresh;
mod conductor_scheduler_targeted_update;
mod conductor_selection_cooldown;
mod conductor_stream_overload_failover;
mod conductor_stream_overload_status;
mod conductor_stream_quota;
mod conductor_subsecond_cooldown;
mod conductor_transport_retry;
mod conductor_unauthorized_refresh;
mod conductor_update;
mod conductor_weight_validation;
mod config_apikey;
mod connection_lifecycle_cooldown;
mod cooldown_backoff;
mod cooldown_state;
mod cooldown_state_store;
mod cooldown_view;
mod credential_policy;
mod download;
mod force_refresh;
mod forced_provider;
mod media_source_formats;
mod meta_refresh;
mod metadata_keys;
mod metadata_merge;
mod oauth_model_alias;
mod openai_compat_pool;
mod persist_policy;
mod response_model_rewriter;
mod response_model_rewriter_antigravity_sim;
mod retry_deadline;
mod scheduler;
mod selector;
mod service_cooldown_store;
mod stream_dropped_queued_failure;
mod stream_request_span;
mod stream_startup_cancel;
mod support;
mod types;
mod types_cooling;
mod websocket_support;
mod weight;
