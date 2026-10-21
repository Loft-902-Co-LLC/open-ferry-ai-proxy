// Ported from CLIProxyAPI internal/cache/claude_web_search_result_cache.go,
// internal/runtime/executor/helps/claude_web_search_results.go and
// claude_executor.go (9d453bc0, cddc9bb8, v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Session-, caller- and credential-scoped native web search results. Only
//! Responses clients opt in. No identities are generated or sent upstream.
//! The store is in-process only; expired entries are swept on access rather
//! than by upstream's background timer. Entry count, not bytes, is bounded.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use open_ferry_core::auth::Auth;
use open_ferry_core::config::{CodexWebSearchResultCacheConfig, Config};
use open_ferry_core::exec::{Format, Options, Request};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::codex::replay::session_key;
use crate::json::str_at;

const RESULT_MARKER: &[u8] = br#""web_search_tool_result""#;

fn contains_marker(data: &[u8]) -> bool {
    data.windows(RESULT_MARKER.len())
        .any(|part| part == RESULT_MARKER)
}

/// A decoded JSON body contains the quoted marker exactly when a key or
/// string value equals it. Avoid serializing another body for this guard.
fn value_contains_marker(value: &Value) -> bool {
    match value {
        Value::String(value) => value == "web_search_tool_result",
        Value::Array(values) => values.iter().any(value_contains_marker),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| key == "web_search_tool_result" || value_contains_marker(value)),
        _ => false,
    }
}

#[cfg(test)]
thread_local! {
    static RECORD_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn parse_record(data: &[u8]) -> Result<Value, serde_json::Error> {
    #[cfg(test)]
    RECORD_PARSES.with(|count| count.set(count.get() + 1));
    serde_json::from_slice(data)
}

#[derive(Clone)]
pub(crate) struct Scope {
    key: String,
    ttl: Duration,
    limit: usize,
    generation: u64,
}

// Matches upstream: file-derived auth IDs survive token replacement, and
// execution: session keys bypass caller prefixes in isolate_session_key.
pub(crate) fn scope(
    config: Option<&Config>,
    auth: &Auth,
    request: &Request,
    options: &Options,
) -> Option<Scope> {
    let config = &config?.client.codex.web_search_result_cache;
    if !config.enabled
        || !options
            .source_format
            .as_str()
            .eq_ignore_ascii_case(Format::OPENAI_RESPONSE.as_str())
    {
        return None;
    }
    let original: Value = serde_json::from_slice(if options.original_request.is_empty() {
        &request.payload
    } else {
        &options.original_request
    })
    .unwrap_or(Value::Null);
    let session = session_key(request, options, &original);
    let session = crate::xai::replay::isolate_session_key(options, &session);
    if session.is_empty() {
        return None;
    }
    // ID, stable index, file name, key/token, then URL: upstream's fallback
    // order, without importing any of the identity-impersonation machinery.
    let identity = if !auth.id.trim().is_empty() {
        auth.id.trim().to_owned()
    } else if !auth.index.trim().is_empty() {
        format!("auth-index|{}", auth.index.trim())
    } else if !auth.file_name.trim().is_empty() {
        format!("auth-file|{}", auth.file_name.trim())
    } else {
        let (key, url) = super::request::credentials(auth);
        if key.trim().is_empty() {
            url.trim().to_owned()
        } else {
            key.trim().to_owned()
        }
    };
    // An unidentified credential cannot safely share encrypted content.
    if identity.is_empty() {
        return None;
    }
    let settings = Settings::from(config);
    let cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    // A stale executor snapshot cannot create a scope after a config change.
    if cache.settings.is_some_and(|current| current != settings) {
        return None;
    }
    Some(Scope {
        generation: cache.generation,
        key: format!("claude:{}:{session}", digest(identity.as_bytes())),
        ttl: config.effective_ttl(),
        limit: config.effective_limit(),
    })
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Apply the authoritative service config at startup and on reload. Dropping
/// all entries on a policy change also prevents old attempts from retaining
/// content or restoring it after a disable/re-enable cycle.
pub fn apply_config(config: &CodexWebSearchResultCacheConfig) {
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .configure(Settings::from(config));
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Settings {
    enabled: bool,
    ttl: Duration,
    limit: usize,
}

impl From<&CodexWebSearchResultCacheConfig> for Settings {
    fn from(config: &CodexWebSearchResultCacheConfig) -> Self {
        Self {
            enabled: config.enabled,
            ttl: config.effective_ttl(),
            limit: config.effective_limit(),
        }
    }
}

struct Entry {
    content: Value,
    expires: Instant,
    used: u64,
}
#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
    clock: u64,
    recency: BTreeSet<(u64, String)>,
    expiry: BTreeSet<(Instant, String)>,
    settings: Option<Settings>,
    generation: u64,
}

impl Cache {
    fn configure(&mut self, settings: Settings) {
        if self.settings == Some(settings) {
            return;
        }
        self.entries.clear();
        self.recency.clear();
        self.expiry.clear();
        self.clock = 0;
        self.generation += 1;
        self.settings = Some(settings);
    }
    fn allows(&self, scope: &Scope) -> bool {
        scope.generation == self.generation
            && self.settings.is_none_or(|settings| {
                settings.enabled && settings.ttl == scope.ttl && settings.limit == scope.limit
            })
    }
    fn key(scope: &Scope, id: &str) -> Option<String> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        Some(digest(format!("{}\0{id}", scope.key.trim()).as_bytes()))
    }
    fn remove(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.recency.remove(&(entry.used, key.to_owned()));
            self.expiry.remove(&(entry.expires, key.to_owned()));
        }
    }
    fn purge_expired(&mut self, now: Instant) {
        while let Some((expires, key)) = self.expiry.first() {
            if *expires > now {
                break;
            }
            let key = key.clone();
            self.remove(&key);
        }
    }
    fn put(&mut self, scope: &Scope, id: &str, content: &Value, now: Instant) {
        if !self.allows(scope) {
            return;
        }
        let Some(key) = Self::key(scope, id) else {
            return;
        };
        self.remove(&key);
        // Each expired entry is removed once; live entries are never scanned.
        self.purge_expired(now);
        self.clock += 1;
        let expires = now + scope.ttl;
        self.recency.insert((self.clock, key.clone()));
        self.expiry.insert((expires, key.clone()));
        self.entries.insert(
            key,
            Entry {
                content: content.clone(),
                expires,
                used: self.clock,
            },
        );
        while self.entries.len() > scope.limit {
            let Some((_, oldest)) = self.recency.first().cloned() else {
                break;
            };
            self.remove(&oldest);
        }
    }
    fn get(&mut self, scope: &Scope, id: &str, now: Instant) -> Option<Value> {
        if !self.allows(scope) {
            return None;
        }
        let key = Self::key(scope, id)?;
        let entry = self.entries.get(&key)?;
        if entry.expires <= now {
            self.remove(&key);
            return None;
        }
        self.recency.remove(&(entry.used, key.clone()));
        self.expiry.remove(&(entry.expires, key.clone()));
        let entry = self.entries.get_mut(&key)?;
        self.clock += 1;
        entry.used = self.clock;
        entry.expires = now + scope.ttl;
        self.recency.insert((entry.used, key.clone()));
        self.expiry.insert((entry.expires, key));
        Some(entry.content.clone())
    }
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

fn empty(content: Option<&Value>) -> bool {
    matches!(content, None | Some(Value::Null))
        || content.is_some_and(|v| v.as_array().is_some_and(Vec::is_empty))
}

impl Scope {
    fn record_block(&self, block: &Value) {
        if str_at(block, "type") != "web_search_tool_result" || empty(block.get("content")) {
            return;
        }
        if let Some(content) = block.get("content") {
            cache().lock().unwrap_or_else(|e| e.into_inner()).put(
                self,
                &str_at(block, "tool_use_id"),
                content,
                Instant::now(),
            );
        }
    }
    pub(crate) fn record(&self, data: &[u8]) {
        if !contains_marker(data) {
            return;
        }
        if let Ok(root) = parse_record(data)
            && let Some(content) = root.get("content").and_then(Value::as_array)
        {
            for block in content {
                self.record_block(block);
            }
            return;
        }
        for line in data.split(|b| *b == b'\n') {
            let line = line.trim_ascii();
            if let Some(data) = line.strip_prefix(b"data:")
                && let Ok(event) = parse_record(data.trim_ascii())
                && str_at(&event, "type") == "content_block_start"
                && let Some(block) = event.get("content_block")
            {
                self.record_block(block);
            }
        }
    }
    pub(crate) fn restore(&self, body: &mut Value) {
        if !value_contains_marker(body) {
            return;
        }
        let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
            return;
        };
        let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        for message in messages {
            if str_at(message, "role") != "assistant" {
                continue;
            }
            let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            for block in content {
                if str_at(block, "type") != "web_search_tool_result" || !empty(block.get("content"))
                {
                    continue;
                }
                let cached = cache.get(self, &str_at(block, "tool_use_id"), now);
                if let Some(content) = cached {
                    block["content"] = content;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope_key(key: &str) -> Scope {
        Scope {
            key: key.to_owned(),
            ttl: Duration::from_secs(1800),
            limit: 1024,
            generation: 0,
        }
    }

    #[test]
    fn marker_free_stream_skips_json_parsing_and_recording() {
        let scope = scope_key("marker-free-stream");
        RECORD_PARSES.with(|count| count.set(0));
        for _ in 0..1000 {
            scope.record(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hello\"}}\n\n");
        }
        scope.record(b"not even JSON");
        let key = Cache::key(&scope, "no-search").unwrap();
        assert!(!cache().lock().unwrap().entries.contains_key(&key));
        RECORD_PARSES.with(|count| assert_eq!(count.get(), 0));
        let mut body =
            json!({"messages":[{"role":"assistant","content":[{"type":"text","text":"hello"}]}]});
        let before = body.clone();
        scope.restore(&mut body);
        assert_eq!(body, before);
        scope.record(br#"{"content":[{"type":"web_search_tool_result","tool_use_id":"no-search","content":[1]}]}"#);
        RECORD_PARSES.with(|count| assert!(count.get() > 0));
        assert!(cache().lock().unwrap().entries.contains_key(&key));
    }

    #[test]
    fn cache_isolates_sessions_and_tool_ids_and_returns_copies() {
        let mut cache = Cache::default();
        let scope = scope_key("session-a");
        let now = Instant::now();
        let content = json!([{"encrypted_content": "dummy"}]);
        cache.put(&scope, "srvtoolu_1", &content, now);
        assert_eq!(cache.get(&scope, "srvtoolu_1", now), Some(content.clone()));
        assert!(
            cache
                .get(&scope_key("session-b"), "srvtoolu_1", now)
                .is_none()
        );
        assert!(cache.get(&scope, "srvtoolu_2", now).is_none());
        assert!(cache.get(&scope, " ", now).is_none());
        let mut got = cache.get(&scope, "srvtoolu_1", now).unwrap();
        got[0]["encrypted_content"] = json!("changed");
        assert_eq!(cache.get(&scope, "srvtoolu_1", now), Some(content));
        assert!(cache.entries.keys().all(|key| key.len() == 64));
    }

    #[test]
    fn expiry_slides_on_read_and_lru_evicts_after_expired_entries() {
        let mut cache = Cache::default();
        let mut scope = scope_key("ttl-lru");
        scope.ttl = Duration::from_secs(60);
        scope.limit = 2;
        let now = Instant::now();
        cache.put(&scope, "short", &json!([1]), now);
        let later = now + Duration::from_secs(59);
        assert!(cache.get(&scope, "short", later).is_some());
        assert!(
            cache
                .get(&scope, "short", later + Duration::from_secs(59))
                .is_some()
        );
        assert!(
            cache
                .get(&scope, "short", later + Duration::from_secs(119))
                .is_none()
        );
        cache.put(&scope, "short", &json!([1]), now);
        scope.ttl = Duration::from_secs(3600);
        cache.put(&scope, "long", &json!([2]), now);
        cache.get(&scope_key("ttl-lru"), "short", now);
        // Replace with short lifetime so the long entry is older but live.
        let mut short = scope.clone();
        short.ttl = Duration::from_secs(1);
        cache.put(&short, "short", &json!([1]), now);
        cache.put(&scope, "new", &json!([3]), now + Duration::from_secs(2));
        assert!(
            cache
                .get(&scope, "long", now + Duration::from_secs(2))
                .is_some()
        );
        cache.put(&scope, "newest", &json!([4]), now + Duration::from_secs(2));
        assert!(
            cache
                .get(&scope, "new", now + Duration::from_secs(2))
                .is_none()
        );
        assert_eq!(cache.entries.len(), 2);
        let large = json!("x".repeat(5 << 20));
        cache.put(&scope, "large", &large, now);
        assert_eq!(cache.get(&scope, "large", now), Some(large));
    }

    #[test]
    fn reads_expire_only_the_requested_entry_and_indexes_stay_bounded() {
        let mut cache = Cache::default();
        let mut scope = scope_key("lazy-expiry");
        scope.ttl = Duration::from_secs(60);
        scope.limit = 2;
        let now = Instant::now();
        cache.put(&scope, "live", &json!([1]), now);
        let mut short = scope.clone();
        short.ttl = Duration::from_secs(1);
        cache.put(&short, "expired", &json!([2]), now);
        let later = now + Duration::from_secs(2);
        for _ in 0..1000 {
            assert!(cache.get(&scope, "live", later).is_some());
        }
        // Reading another key does not sweep the expired one, and repeated
        // sliding reads retain exactly one expiry/recency record per entry.
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.expiry.len(), 2);
        assert_eq!(cache.recency.len(), 2);
        cache.put(&scope, "new", &json!([3]), later);
        assert!(cache.get(&scope, "live", later).is_some());
        assert!(cache.get(&scope, "expired", later).is_none());
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.expiry.len(), 2);
        assert_eq!(cache.recency.len(), 2);
        // Overwriting a key removes its old index records, too.
        cache.put(&scope, "new", &json!([4]), later);
        assert_eq!(cache.get(&scope, "new", later), Some(json!([4])));
        assert_eq!(cache.expiry.len(), 2);
        assert_eq!(cache.recency.len(), 2);
    }

    #[test]
    fn config_changes_purge_entries_and_reject_in_flight_scopes() {
        let mut cache = Cache::default();
        let mut scope = scope_key("config-reload");
        let now = Instant::now();
        let mut settings = Settings {
            enabled: true,
            ttl: scope.ttl,
            limit: scope.limit,
        };
        cache.configure(settings);
        scope.generation = cache.generation;
        cache.put(&scope, "result", &json!([1]), now);
        cache.configure(settings);
        assert_eq!(cache.get(&scope, "result", now), Some(json!([1])));
        // Disable, re-enable, shorten TTL, then shrink the count limit.
        for next in [
            Settings {
                enabled: false,
                ..settings
            },
            settings,
            Settings {
                ttl: Duration::from_secs(1),
                ..settings
            },
            Settings {
                ttl: Duration::from_secs(1),
                limit: 1,
                ..settings
            },
        ] {
            let old = scope.clone();
            cache.configure(next);
            assert!(cache.entries.is_empty());
            assert!(cache.expiry.is_empty());
            assert!(cache.recency.is_empty());
            assert!(cache.get(&old, "result", now).is_none());
            cache.put(&old, "late-result", &json!([2]), now);
            assert!(cache.entries.is_empty());
            scope.generation = cache.generation;
            scope.ttl = next.ttl;
            scope.limit = next.limit;
            cache.put(&scope, "result", &json!([3]), now);
            assert_eq!(cache.get(&scope, "result", now).is_some(), next.enabled);
            settings = next;
        }
        assert!(settings.enabled);
        cache.put(&scope, "new", &json!([4]), now);
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.get(&scope, "result", now).is_none());
        assert!(cache.get(&scope, "new", now + scope.ttl).is_none());
    }

    #[test]
    fn records_messages_and_sse_and_restores_only_empty_assistant_results() {
        for sse in [false, true] {
            let scope = scope_key(if sse { "helper-sse" } else { "helper-message" });
            let results = json!([{"type": "web_search_result", "url": "https://example.com", "encrypted_content": "dummy"}]);
            let block = json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": results});
            let data = if sse {
                format!(
                    "event: content_block_start\ndata: {}\n\n",
                    json!({"type": "content_block_start", "content_block": block})
                )
            } else {
                json!({"content": [block]}).to_string()
            };
            scope.record(data.as_bytes());
            for empty_content in [None, Some(Value::Null), Some(json!([]))] {
                let mut body = json!({"messages": [{"role": "assistant", "content": [
                    {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1"}, {"type": "text", "text": "answer"}]}]});
                if let Some(content) = empty_content {
                    body["messages"][0]["content"][0]["content"] = content;
                }
                scope.restore(&mut body);
                assert_eq!(body["messages"][0]["content"][0]["content"], results);
                assert_eq!(body["messages"][0]["content"][1]["text"], "answer");
            }
            for (role, id, content) in [
                ("user", "srvtoolu_1", json!([])),
                ("assistant", "missing", json!([])),
                (
                    "assistant",
                    "srvtoolu_1",
                    json!([{"encrypted_content": "existing"}]),
                ),
                ("assistant", "srvtoolu_1", json!("")),
            ] {
                let mut body = json!({"messages": [{"role": role, "content": [{"type": "web_search_tool_result", "tool_use_id": id, "content": content}]}]});
                let before = body.clone();
                scope.restore(&mut body);
                assert_eq!(body, before);
            }
            scope.record(b"data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"srvtoolu_1\",\"content\":[]}}");
            let mut body = json!({"messages": [{"role": "assistant", "content": [{"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": []}]}]});
            scope.restore(&mut body);
            assert_eq!(body["messages"][0]["content"][0]["content"], results);
        }
    }

    #[test]
    fn named_sessions_are_isolated_by_caller_and_credential_fallbacks() {
        use open_ferry_core::observe::{Observation, RequestContext};
        use std::sync::Arc;
        let config =
            Config::parse("client: {codex: {web-search-result-cache: {enabled: true}}}").unwrap();
        let request = Request {
            model: "claude-sonnet-5-5".into(),
            payload: bytes::Bytes::from_static(br#"{"prompt_cache_key":"same-session"}"#),
        };
        let opts = |caller: &str| {
            let mut options = Options::new(Format::OPENAI_RESPONSE);
            let context = Arc::new(RequestContext::new(
                http::Method::POST,
                "/v1/responses".to_owned(),
            ));
            context.set_client_key(caller);
            options.observation = Some(Arc::new(Observation::new(context, Vec::new())));
            options
        };
        let a = Auth {
            id: "id-a".into(),
            ..Auth::default()
        };
        assert_ne!(
            scope(Some(&config), &a, &request, &opts("dummy-a"))
                .unwrap()
                .key,
            scope(Some(&config), &a, &request, &opts("dummy-b"))
                .unwrap()
                .key
        );
        let mut key_a = Auth::default();
        key_a.attributes.insert("api_key".into(), "dummy-a".into());
        let mut key_b = key_a.clone();
        key_b.attributes.insert("api_key".into(), "dummy-b".into());
        assert_ne!(
            scope(Some(&config), &key_a, &request, &opts("dummy-a"))
                .unwrap()
                .key,
            scope(Some(&config), &key_b, &request, &opts("dummy-a"))
                .unwrap()
                .key
        );
        assert!(scope(Some(&config), &Auth::default(), &request, &opts("dummy-a")).is_none());
    }

    #[test]
    fn opt_in_requires_responses_a_session_and_credential_and_isolates_credentials() {
        let config =
            Config::parse("client: {codex: {web-search-result-cache: {enabled: true}}}").unwrap();
        let request = Request {
            model: "claude-sonnet-5-5".into(),
            payload: bytes::Bytes::from_static(b"{}"),
        };
        let mut options = Options::new(Format::OPENAI_RESPONSE);
        let auth = Auth {
            id: "credential-a".into(),
            ..Auth::default()
        };
        assert!(scope(Some(&config), &auth, &request, &options).is_none());
        options.metadata.execution_session_id = Some("cache-scope-test".into());
        let a = scope(Some(&config), &auth, &request, &options).unwrap();
        assert_eq!(
            a.key,
            scope(Some(&config), &auth, &request, &options).unwrap().key
        );
        let b = scope(
            Some(&config),
            &Auth {
                id: "credential-b".into(),
                ..Auth::default()
            },
            &request,
            &options,
        )
        .unwrap();
        assert_ne!(a.key, b.key);
        assert!(scope(Some(&Config::default()), &auth, &request, &options).is_none());
        options.source_format = Format::CLAUDE;
        assert!(scope(Some(&config), &auth, &request, &options).is_none());
    }
}
