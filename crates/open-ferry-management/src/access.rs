// Ported from CLIProxyAPI internal/api/handlers/management/handler.go
// (Middleware, AuthenticateManagementKey, purgeStaleAttempts) and
// internal/api/server_management.go (managementAvailable) (v8.0.15, MIT),
// with how golang.org/x/crypto/bcrypt reads a hash (CompareHashAndPassword,
// newFromHash, decodeVersion, decodeCost; v0.54.0, BSD-3-Clause).
// https://github.com/router-for-me/CLIProxyAPI
// https://cs.opensource.google/go/x/crypto

//! Who may use the management API.
//!
//! The API answers only when a management key is set, in the config's
//! `remote-management.secret-key` or in `MANAGEMENT_PASSWORD`; until then
//! every management path is an empty 404, and a config reload turns it on
//! or off. A request then needs the key, as `Authorization: Bearer <key>`,
//! a bare `Authorization` value, or `X-Management-Key`. Clients other than
//! 127.0.0.1 and ::1 also need `remote-management.allow-remote`, which a
//! set `MANAGEMENT_PASSWORD` implies. Five failed attempts from one address
//! ban it for thirty minutes.
//!
//! `secret-key` may be a bcrypt hash or the key itself. Upstream hashes a
//! plain key when it loads the config and writes the hash back to the
//! file; this port never writes the config, and compares a plain key as
//! written instead. A hash is read as Go's bcrypt reads it: whatever
//! follows its 60 characters, such as the line break a YAML block scalar
//! leaves, is ignored.
//!
//! Every answer past the availability check carries `X-CPA-VERSION`,
//! `X-CPA-COMMIT` and `X-CPA-BUILD-DATE`.
//!
//! Deviations from upstream:
//! - The local management password (`SetLocalPassword`), Home mode and
//!   `X-CPA-SUPPORT-PLUGIN` (the plugin host) aren't ported.
//! - `X-CPA-VERSION` is this crate's version. `X-CPA-COMMIT` and
//!   `X-CPA-BUILD-DATE` come from `OPEN_FERRY_COMMIT` and
//!   `OPEN_FERRY_BUILD_DATE` at build time, else `none` and `unknown` as in
//!   an upstream build without them.
//! - A plain `secret-key` is matched in full. Upstream matches its bcrypt
//!   hash, which reads only a key's first 72 bytes, and fails to load a
//!   plain key longer than that.
//! - The failed-attempt record is purged of idle entries when it is next
//!   written, at most hourly, rather than by an hourly timer, and holds at
//!   most 4096 addresses: when it is full, the address least recently
//!   active is forgotten, ending any ban on it early. Upstream's record has
//!   no bound.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::alphabet::BCRYPT;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use subtle::ConstantTimeEq as _;

use crate::client_ip::client_ip;
use crate::go::duration_string;
use crate::json;
use crate::state::ManagementState;

/// Failed attempts that ban an address.
const MAX_FAILURES: u32 = 5;
/// How long a ban lasts.
const BAN_DURATION: Duration = Duration::from_secs(30 * 60);
/// How often idle entries are purged (upstream's `attemptCleanupInterval`).
const PURGE_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// How long an entry may sit idle before it is purged (upstream's
/// `attemptMaxIdleTime`).
const MAX_IDLE: Duration = Duration::from_secs(2 * 60 * 60);
/// The most addresses the record holds.
const MAX_ENTRIES: usize = 4096;
/// Go's bcrypt alphabet, read leniently as Go's `base64.Encoding` reads it
/// and written without padding.
const BCRYPT_BASE64: GeneralPurpose = GeneralPurpose::new(
    &BCRYPT,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// The build headers set on every management answer.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: &str = match option_env!("OPEN_FERRY_COMMIT") {
    Some(commit) => commit,
    None => "none",
};
const BUILD_DATE: &str = match option_env!("OPEN_FERRY_BUILD_DATE") {
    Some(date) => date,
    None => "unknown",
};

/// One address's failed attempts (upstream's `attemptInfo`).
#[derive(Clone, Copy, Debug)]
struct Attempt {
    count: u32,
    blocked_until: Option<Instant>,
    last_activity: Instant,
}

/// Failed attempts by client address.
#[derive(Debug, Default)]
pub(crate) struct Attempts {
    entries: HashMap<String, Attempt>,
    last_purge: Option<Instant>,
}

impl Attempts {
    /// How long `ip` stays banned at `now`, if it is. A ban that has run
    /// out is lifted and the address's count reset.
    fn ban_remaining(&mut self, ip: &str, now: Instant) -> Option<Duration> {
        let attempt = self.entries.get_mut(ip)?;
        let blocked_until = attempt.blocked_until?;
        if now < blocked_until {
            return Some(blocked_until - now);
        }
        attempt.blocked_until = None;
        attempt.count = 0;
        None
    }

    /// Records a failed attempt from `ip`; the fifth bans it.
    fn fail(&mut self, ip: &str, now: Instant) {
        if self
            .last_purge
            .is_none_or(|last| now.duration_since(last) >= PURGE_INTERVAL)
        {
            self.purge(now);
        }
        if !self.entries.contains_key(ip) && self.entries.len() >= MAX_ENTRIES {
            self.purge(now);
            if self.entries.len() >= MAX_ENTRIES {
                self.evict_least_recent();
            }
        }
        let attempt = self.entries.entry(ip.to_owned()).or_insert(Attempt {
            count: 0,
            blocked_until: None,
            last_activity: now,
        });
        attempt.count += 1;
        attempt.last_activity = now;
        if attempt.count >= MAX_FAILURES {
            attempt.blocked_until = Some(now + BAN_DURATION);
            attempt.count = 0;
        }
    }

    /// Clears `ip`'s count and ban after a success.
    fn reset(&mut self, ip: &str) {
        if let Some(attempt) = self.entries.get_mut(ip) {
            attempt.count = 0;
            attempt.blocked_until = None;
        }
    }

    /// Drops entries idle for over two hours whose ban, if any, has ended
    /// (upstream's `purgeStaleAttempts`).
    fn purge(&mut self, now: Instant) {
        self.last_purge = Some(now);
        self.entries.retain(|_, attempt| {
            let banned = attempt.blocked_until.is_some_and(|until| now < until);
            banned || now.saturating_duration_since(attempt.last_activity) <= MAX_IDLE
        });
    }

    fn evict_least_recent(&mut self) {
        let oldest = self
            .entries
            .iter()
            .min_by_key(|(_, attempt)| attempt.last_activity)
            .map(|(ip, _)| ip.clone());
        if let Some(ip) = oldest {
            self.entries.remove(&ip);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Answers an empty 404 while no management key is set (upstream's
/// `managementAvailable`).
pub(crate) async fn availability(
    State(state): State<ManagementState>,
    request: Request,
    next: Next,
) -> Response {
    if !state.available() {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(request).await
}

/// Checks the management key and sets the build headers (upstream's
/// `Middleware`).
pub(crate) async fn authenticate(
    State(state): State<ManagementState>,
    request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    let ip = client_ip(peer, request.headers(), state.trusted_proxies());
    let local = ip == "127.0.0.1" || ip == "::1";
    let provided = provided_key(request.headers()).to_vec();
    let mut response = match authenticate_key(&state, &ip, local, &provided).await {
        Ok(()) => next.run(request).await,
        Err((status, message)) => json::error(status, &message),
    };
    set_build_headers(response.headers_mut());
    response
}

/// The key a request offers: a bearer token, else the whole
/// `Authorization` value, else `X-Management-Key`.
fn provided_key(headers: &HeaderMap) -> &[u8] {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .map_or(&b""[..], HeaderValue::as_bytes);
    let mut provided = authorization;
    if let Some(space) = authorization.iter().position(|&b| b == b' ')
        && authorization[..space].eq_ignore_ascii_case(b"bearer")
    {
        provided = &authorization[space + 1..];
    }
    if provided.is_empty() {
        provided = headers
            .get("x-management-key")
            .map_or(&b""[..], HeaderValue::as_bytes);
    }
    provided
}

fn set_build_headers(headers: &mut HeaderMap) {
    for (name, value) in [
        ("x-cpa-version", VERSION),
        ("x-cpa-commit", COMMIT),
        ("x-cpa-build-date", BUILD_DATE),
    ] {
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(HeaderName::from_static(name), value);
        }
    }
}

/// Whether the client at `ip` may use the API with the key `provided`
/// (upstream's `AuthenticateManagementKey`); if not, the status and
/// message to answer with.
pub(crate) async fn authenticate_key(
    state: &ManagementState,
    ip: &str,
    local: bool,
    provided: &[u8],
) -> Result<(), (StatusCode, String)> {
    let config = state.config();
    let env_secret = state.env_secret();
    let allow_remote = config.remote_management.allow_remote || !env_secret.is_empty();
    let secret = config.remote_management.secret_key.as_str();

    if let Some(remaining) = state.attempts().ban_remaining(ip, Instant::now()) {
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "IP banned due to too many failed attempts. Try again in {}",
                duration_string(round_to_seconds(remaining))
            ),
        ));
    }
    if !local && !allow_remote {
        return Err((StatusCode::FORBIDDEN, "remote management disabled".into()));
    }
    let fail = |message: &str| {
        state.attempts().fail(ip, Instant::now());
        Err((StatusCode::UNAUTHORIZED, message.to_owned()))
    };
    if secret.is_empty() && env_secret.is_empty() {
        return Err((
            StatusCode::FORBIDDEN,
            "remote management key not set".into(),
        ));
    }
    if provided.is_empty() {
        return fail("missing management key");
    }
    if !env_secret.is_empty() && bool::from(provided.ct_eq(env_secret)) {
        state.attempts().reset(ip);
        return Ok(());
    }
    if secret.is_empty() || !key_matches(secret, provided).await {
        return fail("invalid management key");
    }
    state.attempts().reset(ip);
    Ok(())
}

/// Whether `provided` is the configured key: checked against a bcrypt hash
/// off the async threads, or compared in constant time with a plain key.
async fn key_matches(secret: &str, provided: &[u8]) -> bool {
    if !looks_like_bcrypt(secret) {
        return bool::from(provided.ct_eq(secret.as_bytes()));
    }
    let hash = secret.as_bytes().to_vec();
    let provided = provided.to_vec();
    tokio::task::spawn_blocking(move || bcrypt_matches(&hash, &provided))
        .await
        .unwrap_or(false)
}

/// Whether `password` matches the bcrypt `hash`, read as Go's
/// `bcrypt.CompareHashAndPassword` reads it: `$`, a version, two
/// characters `strconv.Atoi` reads as the cost and one more, the
/// 22-character salt, then the hash, of which only the first 31 characters
/// count.
fn bcrypt_matches(hash: &[u8], password: &[u8]) -> bool {
    // Go's minHashSize.
    if hash.len() < 59 || hash[0] != b'$' || hash[1] > b'2' {
        return false;
    }
    let rest = if hash[2] == b'$' {
        &hash[3..]
    } else {
        &hash[4..]
    };
    let Some(cost) = go_atoi(&rest[..2]).filter(|cost| (4..=31).contains(cost)) else {
        return false;
    };
    let (salt, expected) = rest[3..].split_at(22);
    // A shorter hash leaves Go comparing against a zero byte, which no
    // computed hash holds.
    let Some(expected) = expected.get(..31) else {
        return false;
    };
    let Ok(salt) = <[u8; 16]>::try_from(BCRYPT_BASE64.decode(salt).unwrap_or_default()) else {
        return false;
    };
    // Go keys Blowfish with the password and a zero byte, of which it reads
    // the first 72 bytes.
    let mut key = password.to_vec();
    key.push(0);
    key.truncate(72);
    let computed = bcrypt::bcrypt(cost, salt, &key);
    let computed = BCRYPT_BASE64.encode(&computed[..23]);
    computed.as_bytes().ct_eq(expected).into()
}

/// Go's `strconv.Atoi` on a bcrypt cost: an optional sign, then digits.
fn go_atoi(text: &[u8]) -> Option<u32> {
    let digits = match text.first() {
        Some(b'+') => &text[1..],
        _ => text,
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// Whether `secret` reads as a bcrypt hash (upstream's `looksLikeBcrypt`,
/// which decides whether the config loader hashes the key).
fn looks_like_bcrypt(secret: &str) -> bool {
    secret.len() > 4
        && ["$2a$", "$2b$", "$2y$"]
            .iter()
            .any(|p| secret.starts_with(p))
}

/// `duration` rounded to the nearest second, halves up, as Go's
/// `Duration.Round(time.Second)`.
fn round_to_seconds(duration: Duration) -> u64 {
    let nanos = duration.as_nanos() + 500_000_000;
    u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_read_as_upstream_reads_them() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer k1"));
        assert_eq!(provided_key(&headers), b"k1");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("BEARER k 2"),
        );
        assert_eq!(provided_key(&headers), b"k 2");
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(provided_key(&headers), b"Basic abc");
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("raw-key"));
        assert_eq!(provided_key(&headers), b"raw-key");
        // An empty bearer token falls back to X-Management-Key.
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(provided_key(&headers), b"");
        headers.insert("x-management-key", HeaderValue::from_static("k3"));
        assert_eq!(provided_key(&headers), b"k3");
        headers.remove(header::AUTHORIZATION);
        assert_eq!(provided_key(&headers), b"k3");
    }

    #[test]
    fn bans_round_to_the_second() {
        assert_eq!(
            round_to_seconds(Duration::from_millis(29 * 60_000 + 59_499)),
            1799
        );
        assert_eq!(
            round_to_seconds(Duration::from_millis(29 * 60_000 + 59_500)),
            1800
        );
        assert_eq!(round_to_seconds(Duration::from_millis(400)), 0);
    }

    #[test]
    fn five_failures_ban_for_thirty_minutes() {
        let start = Instant::now();
        let mut attempts = Attempts::default();
        for _ in 0..4 {
            attempts.fail("1.2.3.4", start);
        }
        assert_eq!(attempts.ban_remaining("1.2.3.4", start), None);
        attempts.fail("1.2.3.4", start);
        assert_eq!(attempts.ban_remaining("1.2.3.4", start), Some(BAN_DURATION));
        assert_eq!(attempts.ban_remaining("5.6.7.8", start), None);
        let later = start + BAN_DURATION;
        assert_eq!(attempts.ban_remaining("1.2.3.4", later), None);
        // The count started again when the ban ended.
        for _ in 0..4 {
            attempts.fail("1.2.3.4", later);
        }
        assert_eq!(attempts.ban_remaining("1.2.3.4", later), None);
        attempts.reset("1.2.3.4");
        attempts.fail("1.2.3.4", later);
        assert_eq!(attempts.ban_remaining("1.2.3.4", later), None);
    }

    #[test]
    fn idle_entries_are_purged_hourly() {
        let start = Instant::now();
        let mut attempts = Attempts::default();
        attempts.fail("idle", start);
        attempts.fail("recent", start + PURGE_INTERVAL);
        // An hour idle isn't too long.
        assert_eq!(attempts.len(), 2);
        attempts.fail("new", start + PURGE_INTERVAL + Duration::from_secs(30 * 60));
        assert_eq!(attempts.len(), 3);
        // Over two hours idle, and over an hour since the last purge.
        attempts.fail("newer", start + MAX_IDLE + Duration::from_secs(1));
        assert_eq!(attempts.len(), 3);
        assert!(!attempts.entries.contains_key("idle"));
    }

    #[test]
    fn the_record_is_bounded() {
        let start = Instant::now();
        let mut attempts = Attempts::default();
        for i in 0..MAX_ENTRIES {
            attempts.fail(
                &format!("10.0.{}.{}", i / 256, i % 256),
                start + Duration::from_millis(i as u64),
            );
        }
        assert_eq!(attempts.len(), MAX_ENTRIES);
        let now = start + Duration::from_secs(60);
        attempts.fail("192.0.2.1", now);
        assert_eq!(attempts.len(), MAX_ENTRIES);
        assert!(!attempts.entries.contains_key("10.0.0.0"));
        assert!(attempts.entries.contains_key("10.0.0.1"));
        assert!(attempts.entries.contains_key("192.0.2.1"));
    }

    #[test]
    fn bcrypt_hashes_are_read_as_go_reads_them() {
        // Go's bcrypt hash of "review-key" at cost 4.
        let hash = "$2a$04$GEhBc0Dc6cUAAOKCj7D9tux.Of9JT.btaZAYnKgWmvU2aM4fzMhUy";
        let matches = |hash: &str, key: &str| bcrypt_matches(hash.as_bytes(), key.as_bytes());
        assert!(matches(hash, "review-key"));
        assert!(!matches(hash, "review-kez"));
        assert!(!matches(hash, ""));
        // What follows the hash is ignored, as a YAML block scalar's line
        // break is.
        assert!(matches(&format!("{hash}\n"), "review-key"));
        assert!(matches(&format!("{hash}junk"), "review-key"));
        // strconv.Atoi takes a sign, and the byte after the cost is skipped.
        assert!(matches(&hash.replacen("$04$", "$+4$", 1), "review-key"));
        assert!(matches(&hash.replacen("$04$", "$04x", 1), "review-key"));
        assert!(!matches(&hash.replacen("$04$", "$-4$", 1), "review-key"));
        assert!(!matches(&hash.replacen("$04$", "$03$", 1), "review-key"));
        // Too short, or with a broken salt or hash.
        assert!(!matches(&hash[..59], "review-key"));
        assert!(!matches(&hash.replacen("GEhB", "G!hB", 1), "review-key"));
        assert!(!matches(&hash.replacen("MhUy", "MhUz", 1), "review-key"));
        // Keys past 72 bytes count only their first 72, as in Go.
        let long = "k".repeat(80);
        let long_hash = bcrypt::hash_with_salt(&long, 4, [7; 16])
            .unwrap()
            .to_string();
        assert!(matches(&long_hash, &long));
        assert!(matches(&long_hash, &"k".repeat(72)));
        assert!(!matches(&long_hash, &"k".repeat(71)));
    }

    #[test]
    fn bcrypt_hashes_are_recognised_as_upstream_does() {
        assert!(looks_like_bcrypt("$2a$10$abc"));
        assert!(looks_like_bcrypt("$2y$x"));
        assert!(!looks_like_bcrypt("$2a$"));
        assert!(!looks_like_bcrypt("$2x$10$abc"));
        assert!(!looks_like_bcrypt("plain"));
    }
}
