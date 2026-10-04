//! Tests of the routes of `crate::vertex_import`, which no upstream test
//! covers, so each is "Not upstream's". The service accounts carry an RSA
//! key made for the test run, and a token URI that is never reached.

use std::sync::OnceLock;
use std::time::Duration;

use aws_lc_rs::encoding::AsDer as _;
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::KeyPair as _;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Answer, Api, AuthDir, Multipart, SyncCall, keyed};

const IMPORT: &str = "/v0/management/vertex/import";

const EMAIL: &str = "importer@proxy-test.iam.gserviceaccount.com";

/// The PKCS #8 DER of an RSA key made for this test run.
fn test_key_pkcs8() -> &'static [u8] {
    static KEY: OnceLock<Vec<u8>> = OnceLock::new();
    KEY.get_or_init(|| {
        let key = KeyPair::generate(KeySize::Rsa2048).unwrap();
        key.as_der().unwrap().as_ref().to_vec()
    })
}

/// `der` as a PEM block of `kind`, in lines of 64 characters.
fn pem(kind: &str, der: &[u8]) -> String {
    let encoded = STANDARD.encode(der);
    let mut out = format!("-----BEGIN {kind}-----\n");
    for line in encoded.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap());
        out.push('\n');
    }
    out.push_str(&format!("-----END {kind}-----\n"));
    out
}

/// A service account's key file, with the test key in PKCS #8.
fn account() -> Value {
    json!({
        "type": "service_account",
        "project_id": "proxy-test",
        "private_key_id": "kid",
        "private_key": pem("PRIVATE KEY", test_key_pkcs8()),
        "client_email": EMAIL,
        "client_id": "1234",
        "token_uri": "http://127.0.0.1:9/token",
    })
}

/// `account` with `key` set to `value`, or removed for `Value::Null`.
fn account_with(key: &str, value: Value) -> Value {
    let mut account = account();
    let fields = account.as_object_mut().unwrap();
    if value.is_null() {
        fields.remove(key);
    } else {
        fields.insert(key.to_owned(), value);
    }
    account
}

/// Imports key file `contents` at `path`, with field `location` if given.
async fn import_at(api: &Api, path: &str, contents: &[u8], location: Option<&str>) -> Answer {
    let mut form = Multipart::new().file("file", "key.json", contents);
    if let Some(location) = location {
        form = form.text("location", location);
    }
    api.send(form.request(Method::POST, path)).await
}

async fn import(api: &Api, account: &Value) -> Answer {
    import_at(api, IMPORT, account.to_string().as_bytes(), None).await
}

/// The public key of the PKCS #1 key in PEM block `pem`.
fn pkcs1_public_key(pem: &str) -> Vec<u8> {
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = STANDARD.decode(body).unwrap();
    KeyPair::from_der(&der)
        .unwrap()
        .public_key()
        .as_ref()
        .to_vec()
}

fn test_public_key() -> Vec<u8> {
    KeyPair::from_pkcs8(test_key_pkcs8())
        .unwrap()
        .public_key()
        .as_ref()
        .to_vec()
}

// Not upstream's: an import saves the account, its key as PKCS #1, as
// `vertex-<project>.json`, and the service serves it.
#[tokio::test]
async fn imports_a_service_account() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let account = account();

    let answer = import_at(
        &api,
        IMPORT,
        account.to_string().as_bytes(),
        Some(" europe-west4 "),
    )
    .await;

    let path = auth_dir.path().join("vertex-proxy-test.json");
    assert_eq!(
        answer.expect(StatusCode::OK),
        json!({
            "auth-file": path.to_str().unwrap(),
            "email": EMAIL,
            "location": "europe-west4",
            "project_id": "proxy-test",
            "status": "ok",
        })
    );
    let saved = auth_dir.read_json("vertex-proxy-test.json");
    assert_eq!(saved["type"], "vertex");
    assert_eq!(saved["project_id"], "proxy-test");
    assert_eq!(saved["email"], EMAIL);
    assert_eq!(saved["location"], "europe-west4");
    assert_eq!(saved["label"], format!("proxy-test ({EMAIL})"));
    assert_eq!(saved["disabled"], false);
    let key = saved["service_account"]["private_key"].as_str().unwrap();
    assert!(
        key.starts_with("-----BEGIN RSA PRIVATE KEY-----\n"),
        "{key}"
    );
    assert!(key.ends_with("-----END RSA PRIVATE KEY-----\n"), "{key}");
    assert_eq!(pkcs1_public_key(key), test_public_key());
    let mut rest = saved["service_account"].clone();
    rest.as_object_mut().unwrap().remove("private_key");
    let mut expected = account.clone();
    expected.as_object_mut().unwrap().remove("private_key");
    assert_eq!(rest, expected);

    let calls = api.sync.calls();
    assert!(
        matches!(calls.as_slice(), [SyncCall::FileWritten(file)] if file.path == path),
        "{calls:?}"
    );
    let auth = api.manager.get("vertex-proxy-test.json").unwrap();
    assert_eq!(auth.provider, "vertex");
}

// Not upstream's: the region is the form's `location`, else the query's,
// else `us-central1`.
#[tokio::test]
async fn location_falls_back_to_the_query_then_the_default() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let contents = account().to_string();

    for (path, location, want) in [
        (
            format!("{IMPORT}?location=asia-east1"),
            Some("us-east5"),
            "us-east5",
        ),
        (
            format!("{IMPORT}?location=%20asia-east1%20"),
            Some("  "),
            "asia-east1",
        ),
        (format!("{IMPORT}?location=asia-east1"), None, "asia-east1"),
        (IMPORT.to_owned(), None, "us-central1"),
    ] {
        let body = import_at(&api, &path, contents.as_bytes(), location)
            .await
            .expect(StatusCode::OK);
        assert_eq!(body["location"], want, "{path} {location:?}");
        assert_eq!(
            auth_dir.read_json("vertex-proxy-test.json")["location"],
            want
        );
    }
}

// Not upstream's: the v8 route imports for `provider=vertex` alone.
#[tokio::test]
async fn v8_import_dispatches_on_the_provider() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let contents = account().to_string();

    for (query, status, body) in [
        (
            "",
            StatusCode::BAD_REQUEST,
            r#"{"error":"provider is required"}"#,
        ),
        (
            "?provider=%20",
            StatusCode::BAD_REQUEST,
            r#"{"error":"provider is required"}"#,
        ),
        (
            "?provider=claude",
            StatusCode::NOT_FOUND,
            r#"{"error":"provider_not_found"}"#,
        ),
        (
            "?provider=gemini",
            StatusCode::NOT_FOUND,
            r#"{"error":"provider_not_found"}"#,
        ),
    ] {
        let path = format!("/v8/management/oauth/import{query}");
        import_at(&api, &path, contents.as_bytes(), None)
            .await
            .assert(status, body);
    }
    assert!(api.sync.calls().is_empty());

    let path = "/v8/management/oauth/import?provider=%20Vertex%20&location=us-east1";
    let body = import_at(&api, path, contents.as_bytes(), None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(body["location"], "us-east1");
    assert_eq!(
        auth_dir.read_json("vertex-proxy-test.json")["type"],
        "vertex"
    );
}

// Not upstream's: a key file that isn't a JSON object.
#[tokio::test]
async fn invalid_json_is_refused() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);

    let body = import_at(&api, IMPORT, br#"{"project_id":"#, None)
        .await
        .expect(StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid json");
    assert!(body["message"].as_str().is_some_and(|m| !m.is_empty()));
    import_at(&api, IMPORT, b"[1]", None).await.assert(
        StatusCode::BAD_REQUEST,
        concat!(
            r#"{"error":"invalid json","message":"json: cannot unmarshal array into Go value "#,
            r#"of type map[string]interface {}"}"#,
        ),
    );
    import_at(&api, IMPORT, b"null", None).await.assert(
        StatusCode::BAD_REQUEST,
        r#"{"error":"invalid service account","message":"service account payload is empty"}"#,
    );
    assert!(api.sync.calls().is_empty());
}

// Not upstream's: the key file is read with a JSON parser that takes at
// most 127 levels of nesting, the account's own object the first, where Go
// takes 10000. A deeper file answers 400 `invalid json`. The saved file
// holds the account a level deeper, under `service_account`, and the service
// reads it with the same limit, so an account 127 deep is saved but not
// served.
#[tokio::test]
async fn deeply_nested_accounts() {
    /// `arrays` arrays nested around `0`.
    fn nested(arrays: usize) -> Value {
        (0..arrays).fold(json!(0), |value, _| json!([value]))
    }

    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let body = import(&api, &account_with("extra", nested(127)))
        .await
        .expect(StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid json");
    let message = body["message"].as_str().unwrap();
    assert!(message.starts_with("recursion limit exceeded"), "{message}");
    assert!(std::fs::read_dir(auth_dir.path()).unwrap().next().is_none());
    assert!(api.sync.calls().is_empty());

    for (arrays, served) in [(125, true), (126, false)] {
        let auth_dir = AuthDir::new();
        let api = Api::over(&auth_dir);
        import(&api, &account_with("extra", nested(arrays)))
            .await
            .expect(StatusCode::OK);
        let saved = auth_dir.path().join("vertex-proxy-test.json");
        assert!(saved.is_file(), "{arrays}");
        assert_eq!(api.sync.calls().len(), 1, "{arrays}");
        assert_eq!(
            api.manager.get("vertex-proxy-test.json").is_some(),
            served,
            "{arrays}"
        );
    }
}

// Not upstream's: an account without a usable RSA key is refused, and the
// answer never quotes the key.
#[tokio::test]
async fn invalid_service_accounts_are_refused() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);

    for value in [Value::Null, json!(" "), json!(7)] {
        import(&api, &account_with("private_key", value)).await.assert(
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid service account","message":"service account missing private_key"}"#,
        );
    }
    let key = pem("PRIVATE KEY", test_key_pkcs8());
    let encoded = STANDARD.encode(test_key_pkcs8());
    let corrupt = key.replacen(&encoded[100..110], "!!!!!!!!!!", 1);
    for value in [
        json!("not a key"),
        json!(corrupt),
        json!(pem("CERTIFICATE", b"x")),
    ] {
        let answer = import(&api, &account_with("private_key", value)).await;
        let body = answer.expect(StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid service account");
        let message = body["message"].as_str().unwrap();
        assert!(!message.is_empty());
        assert!(!answer.body.contains(&encoded[..40]), "{}", answer.body);
        assert!(!answer.body.contains(&encoded[200..240]), "{}", answer.body);
    }
    assert!(api.sync.calls().is_empty());
    assert!(std::fs::read_dir(auth_dir.path()).unwrap().next().is_none());
}

// Not upstream's: an account must have a `project_id`.
#[tokio::test]
async fn project_id_is_required() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);

    for value in [Value::Null, json!(""), json!("  ")] {
        import(&api, &account_with("project_id", value))
            .await
            .assert(StatusCode::BAD_REQUEST, r#"{"error":"project_id missing"}"#);
    }
    assert!(api.sync.calls().is_empty());
}

// Not upstream's: a `project_id` that isn't a string reads as Go prints it,
// and names the file with `/`, `\`, `:` and spaces replaced.
#[tokio::test]
async fn project_ids_name_the_file() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);

    for (project_id, want, file) in [
        (json!(123), "123", "vertex-123.json"),
        (json!(1234567), "1.234567e+06", "vertex-1.234567e+06.json"),
        (json!(0.00001), "1e-05", "vertex-1e-05.json"),
        (json!(true), "true", "vertex-true.json"),
        (json!([1, "a"]), "[1 a]", "vertex-[1-a].json"),
        (
            json!({"b": 2, "a": [false]}),
            "map[a:[false] b:2]",
            "vertex-map[a_[false]-b_2].json",
        ),
        (json!(" a/b\\c:d e "), "a/b\\c:d e", "vertex-a_b_c_d-e.json"),
    ] {
        let body = import(&api, &account_with("project_id", project_id))
            .await
            .expect(StatusCode::OK);
        assert_eq!(body["project_id"], want);
        let path = auth_dir.path().join(file);
        assert_eq!(body["auth-file"], path.to_str().unwrap());
        assert_eq!(auth_dir.read_json(file)["project_id"], want);
    }
}

// Not upstream's: a `project_id` that would name a file Windows can't hold
// is refused, on every system; a map holding null prints as `<nil>`.
#[tokio::test]
async fn unsafe_project_ids_are_refused() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);

    for project_id in [
        json!("bad?id"),
        json!("a*b"),
        json!("x|y"),
        json!("q\"r"),
        json!("tab\tbed"),
        json!({"a": null}),
    ] {
        import(&api, &account_with("project_id", project_id))
            .await
            .assert(
                StatusCode::BAD_REQUEST,
                concat!(
                    r#"{"error":"invalid service account","#,
                    r#""message":"project_id holds characters a file name can't"}"#,
                ),
            );
    }
    assert!(std::fs::read_dir(auth_dir.path()).unwrap().next().is_none());
}

// Not upstream's: the key file must be sent as field `file` of a form.
#[tokio::test]
async fn the_file_is_required() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let required = r#"{"error":"file required"}"#;
    let contents = account().to_string();

    let request = Multipart::new()
        .text("location", "us-east1")
        .text("file", &contents)
        .request(Method::POST, IMPORT);
    api.send(request)
        .await
        .assert(StatusCode::BAD_REQUEST, required);
    let request = Multipart::new()
        .file("key", "key.json", contents.as_bytes())
        .request(Method::POST, IMPORT);
    api.send(request)
        .await
        .assert(StatusCode::BAD_REQUEST, required);
    api.send(keyed(Method::POST, IMPORT, &contents))
        .await
        .assert(StatusCode::BAD_REQUEST, required);

    let large = vec![b' '; (8 << 20) + 1];
    import_at(&api, IMPORT, &large, None).await.assert(
        StatusCode::PAYLOAD_TOO_LARGE,
        r#"{"error":"auth file too large"}"#,
    );
    let huge = vec![b' '; (32 << 20) + 1];
    import_at(&api, IMPORT, &huge, None).await.assert(
        StatusCode::PAYLOAD_TOO_LARGE,
        r#"{"error":"request body too large"}"#,
    );
    assert!(api.sync.calls().is_empty());
}

// Not upstream's: without a credential store the route answers 503; when
// the service has stopped, the file is saved and the route answers 503.
#[tokio::test]
async fn store_and_service_unavailable() {
    let api = Api::new();
    import(&api, &account()).await.assert(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"credential store unavailable"}"#,
    );

    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    api.sync.stop();
    import(&api, &account()).await.assert(
        StatusCode::SERVICE_UNAVAILABLE,
        concat!(
            r#"{"error":"save_failed","message":"post-auth persist hook failed: "#,
            r#"credential sync unavailable: the service has stopped"}"#,
        ),
    );
    assert_eq!(
        auth_dir.read_json("vertex-proxy-test.json")["project_id"],
        "proxy-test"
    );
}

// Not upstream's: an import never saves through a directory junction (on
// Windows, where anyone may make one) or a symlink.
#[tokio::test]
async fn imports_never_write_through_a_link() {
    let auth_dir = AuthDir::new();
    let target = auth_dir.path().parent().unwrap().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = auth_dir.path().join("vertex-proxy-test.json");
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&target, &link).is_ok();
    #[cfg(windows)]
    let made = std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(&link)
        .arg(&target)
        .output()
        .is_ok_and(|output| output.status.success());
    if !made {
        eprintln!("skipped: can't make a link here");
        return;
    }
    let api = Api::over(&auth_dir);

    let body = import(&api, &account())
        .await
        .expect(StatusCode::INTERNAL_SERVER_ERROR);

    assert_eq!(
        body,
        json!({
            "error": "save_failed",
            "message": format!("{} is a symlink", link.display()),
        })
    );
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    assert!(api.sync.calls().is_empty());
}

// Not upstream's: an import waits while the credential lock is held, as a
// status change holds it, and saves nothing until it is free.
#[tokio::test]
async fn imports_wait_for_the_credential_lock() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let account = account();
    let guard = api.state.credential_lock().lock().await;

    let mut pending = std::pin::pin!(import(&api, &account));
    let waited = tokio::time::timeout(Duration::from_millis(500), &mut pending).await;
    assert!(waited.is_err(), "the import didn't wait for the lock");
    assert!(std::fs::read_dir(auth_dir.path()).unwrap().next().is_none());
    assert!(api.sync.calls().is_empty());

    drop(guard);
    pending.await.expect(StatusCode::OK);
    assert_eq!(
        auth_dir.read_json("vertex-proxy-test.json")["project_id"],
        "proxy-test"
    );
    assert_eq!(api.sync.calls().len(), 1);
}

// Not upstream's: an import tells the service while it holds the
// credential lock, so the change takes its revision under the lock, and
// awaits the service once the lock is released.
#[tokio::test]
async fn imports_are_sent_under_the_lock_and_awaited_after() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    api.sync.watch_lock(api.state.clone());

    import(&api, &account()).await.expect(StatusCode::OK);

    assert_eq!(api.sync.calls().len(), 1);
    assert_eq!(api.sync.lock_held(), [(true, false)]);
}
