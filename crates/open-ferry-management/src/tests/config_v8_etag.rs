// Ported from CLIProxyAPI internal/api/handlers/management/
// config_v8_etag_test.go (TestConfigV8ETagIfMatch) (v8.0.32, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The version token of the v8 config routes (see
//! [`crate::config_etag`]): the `ETag` of a read, and `If-Match` on a write,
//! over a config file in a temporary directory.
//!
//! Deviations from upstream's test: it calls the handler on gin's test
//! router; here the requests go through the management router, with the
//! management key. Its `TestConfigV8ETagExposedForCORS` runs the real
//! server, which this crate isn't; the exposed header is checked in
//! `open-ferry-server`.

use http::{HeaderValue, Method, StatusCode, header};
use open_ferry_core::config::Config;
use open_ferry_core::config::save::sha256_hex;

use super::{Answer, Api, AuthDir, keyed};

const RAW: &str = "config-version: 8\nserver: {port: 8317}\naccess: {api-keys: [revoked-key]}\n";

/// `method path` with `body` and one `If-Match` line per entry of `lines`.
async fn send(api: &Api, method: Method, path: &str, body: &str, lines: &[&str]) -> Answer {
    let mut request = keyed(method, path, body);
    for line in lines {
        request
            .headers_mut()
            .append(header::IF_MATCH, HeaderValue::from_str(line).unwrap());
    }
    api.send(request).await
}

/// `method path` with `body` and the single `If-Match` line `if_match`
/// (none when empty).
async fn call(api: &Api, method: Method, path: &str, body: &str, if_match: &str) -> Answer {
    if if_match.is_empty() {
        send(api, method, path, body, &[]).await
    } else {
        send(api, method, path, body, &[if_match]).await
    }
}

fn file_bytes(dir: &AuthDir) -> String {
    std::fs::read_to_string(dir.config_path()).unwrap()
}

/// The tag of the file as it is now.
fn file_etag(dir: &AuthDir) -> String {
    format!("\"{}\"", sha256_hex(file_bytes(dir).as_bytes()))
}

fn status(answer: &Answer, status: StatusCode) {
    assert_eq!(answer.status, status, "{}", answer.body);
}

/// Upstream's `TestConfigV8ETagIfMatch`: a stale whole-document write can't
/// resurrect a change another writer saved.
#[tokio::test]
async fn etag_and_if_match_fence_v8_writes() {
    let dir = AuthDir::new();
    let api = Api::over_config_file(&dir, RAW);
    let config = "/v8/management/config";
    let yaml = "/v8/management/config.yaml";
    let keys = "/v8/management/config/access/api-keys";
    let port = "/v8/management/config/server/port";

    // PATCH at the root with a stale ETag fails with 412.
    let answer = call(
        &api,
        Method::PATCH,
        config,
        r#"{"server":{"port":8319}}"#,
        "\"stale\"",
    )
    .await;
    status(&answer, StatusCode::PRECONDITION_FAILED);
    assert_eq!(answer.body, r#"{"error":"precondition_failed"}"#);
    assert_eq!(file_bytes(&dir), RAW, "a stale PATCH changed the file");

    // PATCH with the current ETag goes through and returns the new one.
    let current = file_etag(&dir);
    let answer = call(
        &api,
        Method::PATCH,
        config,
        r#"{"server":{"port":8319}}"#,
        &current,
    )
    .await;
    status(&answer, StatusCode::OK);
    assert_eq!(answer.header("etag"), Some(file_etag(&dir).as_str()));
    assert_ne!(file_etag(&dir), current);
    assert_eq!(Config::load(dir.config_path()).unwrap().port, 8319);

    // A read advertises the on-disk version as a strong ETag.
    let get = api.get(yaml).await;
    status(&get, StatusCode::OK);
    let etag = get.header("etag").unwrap().to_owned();
    assert_eq!(etag, file_etag(&dir));
    let sub = api.get("/v8/management/config/server").await;
    status(&sub, StatusCode::OK);
    assert_eq!(sub.header("etag"), Some(etag.as_str()));
    let whole = api.get(config).await;
    status(&whole, StatusCode::OK);
    assert_eq!(whole.header("etag"), Some(etag.as_str()));

    // Another writer revokes the API key between that read and our write.
    let revoked = call(&api, Method::PUT, keys, r#"["other-key"]"#, "").await;
    status(&revoked, StatusCode::OK);
    assert_eq!(revoked.header("etag"), Some(file_etag(&dir).as_str()));

    // A whole-document PUT based on the stale read doesn't resurrect it.
    let before = file_bytes(&dir);
    let stale = call(&api, Method::PUT, yaml, RAW, &etag).await;
    status(&stale, StatusCode::PRECONDITION_FAILED);
    assert_eq!(stale.header("etag"), Some(file_etag(&dir).as_str()));
    assert_eq!(file_bytes(&dir), before, "a stale PUT changed the file");
    assert_eq!(
        Config::load(dir.config_path()).unwrap().api_keys,
        ["other-key"]
    );

    // DELETE with a stale ETag is refused the same way.
    let answer = call(&api, Method::DELETE, keys, "", &etag).await;
    status(&answer, StatusCode::PRECONDITION_FAILED);
    assert_eq!(file_bytes(&dir), before, "a stale DELETE changed the file");

    // Weak and malformed validators never match under strong comparison, a
    // comma inside quotes is tag content, and only the whole field is the
    // wildcard.
    let weak = format!("W/{}", file_etag(&dir));
    for if_match in [
        weak.as_str(),
        "not-an-etag",
        "\"unterminated",
        "\"a\"b\"",
        "\"stale,*,tag\"",
        "W/\"stale,*,tag\"",
        "\"stale\", *",
    ] {
        let answer = call(&api, Method::PUT, yaml, &before, if_match).await;
        status(&answer, StatusCode::PRECONDITION_FAILED);
    }

    // Tags split across repeated header lines are one list.
    let current = file_etag(&dir);
    let answer = send(&api, Method::PUT, port, "8318", &["\"stale\"", &current]).await;
    status(&answer, StatusCode::OK);

    // A quoted current hash with a trailing comma inside the quotes is a
    // different tag.
    let hash = sha256_hex(file_bytes(&dir).as_bytes());
    let answer = call(&api, Method::PUT, port, "8317", &format!("\"{hash},\"")).await;
    status(&answer, StatusCode::PRECONDITION_FAILED);

    // A header that is there but blank carries no tag and fails.
    for blank in ["", "  \t  "] {
        let answer = send(&api, Method::PUT, port, "8317", &[blank]).await;
        status(&answer, StatusCode::PRECONDITION_FAILED);
    }

    // A list holding the current tag, with whitespace and weak members.
    let current = file_etag(&dir);
    let list = format!("\"stale\", {current} , W/{current}");
    let answer = call(&api, Method::DELETE, keys, "", &list).await;
    status(&answer, StatusCode::OK);
    assert!(Config::load(dir.config_path()).unwrap().api_keys.is_empty());

    // The wildcard matches any existing version.
    let answer = call(&api, Method::PUT, port, "8320", "*").await;
    status(&answer, StatusCode::OK);

    // A matching ETag chains: the write's ETag is the next write's.
    let answer = call(&api, Method::PUT, port, "8321", &file_etag(&dir)).await;
    status(&answer, StatusCode::OK);
    let next = answer.header("etag").unwrap().to_owned();
    assert_eq!(next, file_etag(&dir));
    let answer = call(&api, Method::PUT, port, "8322", &next).await;
    status(&answer, StatusCode::OK);

    // Without If-Match a write is unconditional.
    let answer = call(&api, Method::PUT, port, "8323", "").await;
    status(&answer, StatusCode::OK);

    // A read of a missing key is a 404 without an ETag.
    let missing = api.get("/v8/management/config/no-such-key").await;
    status(&missing, StatusCode::NOT_FOUND);
    assert_eq!(missing.header("etag"), None);
}

// Not upstream's: a write refused before it is made (a path that isn't a
// key, a DELETE of nothing) is still checked against If-Match first, as
// upstream checks it before reading the path or the body; and a 412 leaves
// the reload and the handlers' config alone.
#[tokio::test]
async fn if_match_is_checked_before_the_write_is_judged() {
    let dir = AuthDir::new();
    let api = Api::over_config_file(&dir, RAW);
    let delete_whole = "/v8/management/config/";
    let missing = "/v8/management/config/no-such-key";

    // Matching: the write's own answer.
    let current = file_etag(&dir);
    let answer = call(&api, Method::DELETE, missing, "", &current).await;
    status(&answer, StatusCode::NOT_FOUND);
    let answer = call(&api, Method::DELETE, delete_whole, "", &current).await;
    status(&answer, StatusCode::BAD_REQUEST);

    // Stale: 412, with the current tag.
    for path in [missing, delete_whole] {
        let answer = call(&api, Method::DELETE, path, "", "\"stale\"").await;
        status(&answer, StatusCode::PRECONDITION_FAILED);
        assert_eq!(answer.header("etag"), Some(current.as_str()));
    }
    let answer = call(
        &api,
        Method::PUT,
        "/v8/management/config/server",
        "not json",
        "\"stale\"",
    )
    .await;
    status(&answer, StatusCode::PRECONDITION_FAILED);
    let answer = call(
        &api,
        Method::PUT,
        "/v8/management/config/server",
        "not json",
        &current,
    )
    .await;
    status(&answer, StatusCode::BAD_REQUEST);
    assert_eq!(file_bytes(&dir), RAW);
    assert!(*api.state.config() == Config::load(dir.config_path()).unwrap());
}

// Not upstream's: a file that can't be read answers read_failed to a write
// with If-Match, as upstream does before it compares.
#[tokio::test]
async fn if_match_on_a_missing_file_is_read_failed() {
    let dir = AuthDir::new();
    let api = Api::over_config_file(&dir, RAW);
    std::fs::remove_file(dir.config_path()).unwrap();
    let answer = call(
        &api,
        Method::PUT,
        "/v8/management/config/server/port",
        "8318",
        "*",
    )
    .await;
    status(&answer, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(answer.body, r#"{"error":"read_failed"}"#);
}

/// A writer that changes the config file on disk, as `open-ferry config`
/// does from another process, just before it takes the file's lock: after
/// the handler's own checks, before the locked read.
struct ChangesBeforeTheLock {
    inner: crate::FileConfigWriter,
    path: std::path::PathBuf,
    contents: &'static str,
}

impl crate::ConfigWriter for ChangesBeforeTheLock {
    fn save_preserving_comments(
        &self,
        config: &Config,
        migrate_v8: bool,
        expected: Option<&str>,
    ) -> Result<Option<String>, crate::WriteError> {
        self.inner
            .save_preserving_comments(config, migrate_v8, expected)
    }

    fn write_file(&self, data: &[u8]) -> Result<Option<String>, crate::WriteError> {
        self.inner.write_file(data)
    }

    fn edit_v8(
        &self,
        edit: &crate::V8Edit,
        condition: Option<&crate::V8Precondition>,
    ) -> Result<(Config, Option<String>), crate::V8EditError> {
        std::fs::write(&self.path, self.contents).unwrap();
        self.inner.edit_v8(edit, condition)
    }

    fn undo(&self, check: &crate::UndoCheck) -> Result<(Config, Option<String>), crate::UndoError> {
        self.inner.undo(check)
    }
}

// Not upstream's: the file changes after the handler's own check, as a save
// by another process can; the writer checks `If-Match` again on the bytes it
// reads under the file's lock, so the write is a 412 with the new file's tag
// and changes nothing.
#[tokio::test]
async fn if_match_is_checked_under_the_files_lock() {
    const OTHER: &str = "config-version: 8\nserver: {port: 8317}\naccess: {api-keys: []}\n";
    let dir = AuthDir::new();
    let mut api = Api::over_config_file(&dir, RAW);
    let state = api
        .state
        .clone()
        .with_config_writer(std::sync::Arc::new(ChangesBeforeTheLock {
            inner: crate::FileConfigWriter::new(dir.config_path()),
            path: dir.config_path(),
            contents: OTHER,
        }));
    api.router = crate::router(state.clone());
    api.state = state;

    // The client's tag is the file's when the handler looks, and stale when
    // the writer reads.
    let answer = call(
        &api,
        Method::PUT,
        "/v8/management/config.yaml",
        RAW,
        &file_etag(&dir),
    )
    .await;
    status(&answer, StatusCode::PRECONDITION_FAILED);
    assert_eq!(answer.body, r#"{"error":"precondition_failed"}"#);
    assert_eq!(file_bytes(&dir), OTHER, "the stale write changed the file");
    assert_eq!(answer.header("etag"), Some(file_etag(&dir).as_str()));
}

/// A writer whose first write is caught half done, as an in-place write
/// that has truncated the file and not yet written it: it truncates the file,
/// says so, waits to be let go, and puts the file back as it was.
struct HalfDone {
    inner: crate::FileConfigWriter,
    path: std::path::PathBuf,
    contents: &'static str,
    first: std::sync::atomic::AtomicBool,
    truncated: std::sync::Mutex<std::sync::mpsc::Sender<()>>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl crate::ConfigWriter for HalfDone {
    fn save_preserving_comments(
        &self,
        config: &Config,
        migrate_v8: bool,
        expected: Option<&str>,
    ) -> Result<Option<String>, crate::WriteError> {
        self.inner
            .save_preserving_comments(config, migrate_v8, expected)
    }

    fn write_file(&self, data: &[u8]) -> Result<Option<String>, crate::WriteError> {
        self.inner.write_file(data)
    }

    fn edit_v8(
        &self,
        edit: &crate::V8Edit,
        condition: Option<&crate::V8Precondition>,
    ) -> Result<(Config, Option<String>), crate::V8EditError> {
        if !self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return self.inner.edit_v8(edit, condition);
        }
        std::fs::write(&self.path, "").unwrap();
        self.truncated.lock().unwrap().send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        std::fs::write(&self.path, self.contents).unwrap();
        let config = Config::load_bytes(self.contents.as_bytes()).unwrap();
        Ok((config, Some(sha256_hex(self.contents.as_bytes()))))
    }

    fn undo(&self, check: &crate::UndoCheck) -> Result<(Config, Option<String>), crate::UndoError> {
        self.inner.undo(check)
    }
}

// Not upstream's: a write that leaves the file as it was, caught between
// truncating it and writing it, doesn't make the next request's matching tag
// stale. Upstream's handler mutex makes that request wait for the write; the
// early check waits for the API's write lock as well, and is not given the
// empty file.
#[tokio::test]
async fn a_matching_tag_waits_for_a_write_half_done() {
    let dir = AuthDir::new();
    let mut api = Api::over_config_file(&dir, RAW);
    let tag = file_etag(&dir);
    let (truncated, saw_truncated) = std::sync::mpsc::channel();
    let (let_go, release) = std::sync::mpsc::channel();
    let state = api
        .state
        .clone()
        .with_config_writer(std::sync::Arc::new(HalfDone {
            inner: crate::FileConfigWriter::new(dir.config_path()),
            path: dir.config_path(),
            contents: RAW,
            first: std::sync::atomic::AtomicBool::new(true),
            truncated: std::sync::Mutex::new(truncated),
            release: std::sync::Mutex::new(release),
        }));
    api.router = crate::router(state.clone());
    api.state = state;

    let port = "/v8/management/config/server/port";
    let first = call(&api, Method::PUT, port, "8317", &tag);
    let second = async {
        tokio::task::spawn_blocking(move || saw_truncated.recv().unwrap())
            .await
            .unwrap();
        assert_eq!(file_bytes(&dir), "", "the first write is half done");
        let second = call(&api, Method::PUT, port, "8318", &tag);
        let release = async {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let_go.send(()).unwrap();
        };
        tokio::join!(second, release).0
    };
    let (first, second) = tokio::join!(first, second);
    status(&first, StatusCode::OK);
    assert_eq!(first.header("etag"), Some(tag.as_str()));
    status(&second, StatusCode::OK);
    assert!(file_bytes(&dir).contains("8318"), "{}", file_bytes(&dir));
}

// Not upstream's: two writes with the same tag, at once: the first changes
// the file, so the second's tag is stale by the time it reads.
#[tokio::test]
async fn two_writes_with_one_tag_have_one_winner() {
    let dir = AuthDir::new();
    let api = Api::over_config_file(&dir, RAW);
    let tag = file_etag(&dir);
    let port = "/v8/management/config/server/port";
    let (a, b) = tokio::join!(
        call(&api, Method::PUT, port, "8318", &tag),
        call(&api, Method::PUT, port, "8319", &tag),
    );
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::PRECONDITION_FAILED],
        "{a:?} {b:?}"
    );
    let loser = if a.status == StatusCode::OK { &b } else { &a };
    assert_eq!(loser.header("etag"), Some(file_etag(&dir).as_str()));
}

// Not upstream's: upstream checks `If-Match` before it reads the body, so a
// stale tag beats a body over the limit (413).
#[tokio::test]
async fn a_stale_tag_beats_a_body_over_the_limit() {
    let dir = AuthDir::new();
    let api = Api::over_config_file(&dir, RAW);
    let big = " ".repeat((16 << 20) + 1);
    let answer = call(
        &api,
        Method::PUT,
        "/v8/management/config.yaml",
        &big,
        "\"stale\"",
    )
    .await;
    status(&answer, StatusCode::PRECONDITION_FAILED);
    assert_eq!(answer.header("etag"), Some(file_etag(&dir).as_str()));
    // With the current tag the body is read, and refused as too large.
    let answer = call(
        &api,
        Method::PUT,
        "/v8/management/config.yaml",
        &big,
        &file_etag(&dir),
    )
    .await;
    status(&answer, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(file_bytes(&dir), RAW);
}

// Not upstream's: a writer that keeps no hash gives no ETag, rather than one
// read back from the file outside the lock.
#[tokio::test]
async fn a_writer_without_a_hash_gives_no_etag() {
    let api = Api::writing(super::keyed_config());
    let answer = api
        .call(Method::PUT, "/v8/management/config/server/port", "8318")
        .await;
    status(&answer, StatusCode::OK);
    assert_eq!(answer.header("etag"), None);
}
