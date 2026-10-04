// Ported from CLIProxyAPI internal/api/handlers/management/
// auth_files_upload_test.go, auth_files_batch_test.go,
// auth_files_delete_test.go, auth_files_download_test.go and
// auth_files_download_windows_test.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Tests of the routes of `crate::credential_files`: downloading,
//! uploading and deleting credential files. Every test of upstream's five
//! files is ported, each request going through the router with the
//! management key where upstream calls the handler.
//!
//! Changed from upstream:
//! - `TestUploadAuthFile_InvokesPostAuthPersistHook` checks the file sent
//!   to the [`FakeSync`](super::FakeSync), which stands in for the hook.
//! - `TestDeleteAuthFile_UsesAuthPathFromManager` expects the delete to be
//!   refused with 409, keeping both files and the credential: a credential
//!   whose file is outside the auth directory isn't deleted here.
//!
//! The tests marked "Not upstream's" cover the rest: names that could
//! reach outside the auth directory, case on Windows, symlinks and
//! junctions, which directory a credential's file is in, sizes, partial
//! batches, `?all=true`, delete bodies, a store or service that isn't
//! there, a form's `Debug`, and how a part's names are read from its
//! `Content-Disposition`, checked against Go's answers.

use std::io;
use std::path::{Path, PathBuf};

use http::{Method, StatusCode};
use serde_json::json;

use super::{
    Answer, Api, AuthDir, LOCAL, Multipart, SyncCall, auth, file_auth, keyed, request_from,
};
use crate::credential_files::{is_unsafe_name, part_names, read_form};

const CODEX: &str = r#"{"type":"codex","email":"user@example.com"}"#;

const OTHER: &str = r#"{"type":"codex","email":"other@example.com"}"#;

const OK: &str = r#"{"status":"ok"}"#;

const INVALID_NAME: &str = r#"{"error":"invalid name"}"#;

/// `name` percent-encoded for a query.
fn encode(name: &str) -> String {
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A form uploading each file as field `file`.
fn form(files: &[(&str, &[u8])]) -> Multipart {
    files
        .iter()
        .fold(Multipart::new(), |form, (name, contents)| {
            form.file("file", name, contents)
        })
}

/// Uploads `files` in a form.
async fn upload(api: &Api, files: &[(&str, &str)]) -> Answer {
    let files: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(name, contents)| (*name, contents.as_bytes()))
        .collect();
    let request = form(&files).request(Method::POST, "/v0/management/auth-files");
    api.send(request).await
}

/// Uploads `body` as file `name`, without a form.
async fn upload_raw(api: &Api, name: &str, body: &str) -> Answer {
    let path = format!("/v0/management/auth-files?name={}", encode(name));
    api.post(&path, body).await
}

/// `DELETE /v0/management/auth-files` with `query` (empty, or `?` and a
/// query) and `body`.
async fn delete(api: &Api, query: &str, body: &str) -> Answer {
    let path = format!("/v0/management/auth-files{query}");
    api.send(keyed(Method::DELETE, &path, body)).await
}

/// Deletes the file or credential `name`.
async fn delete_name(api: &Api, name: &str) -> Answer {
    delete(api, &format!("?name={}", encode(name)), "").await
}

async fn download(api: &Api, name: &str) -> Answer {
    let path = format!("/v0/management/auth-files/download?name={}", encode(name));
    api.get(&path).await
}

/// A directory `external` beside the auth directory, holding
/// `secret.json`, whose path is returned.
fn external_secret(auth_dir: &AuthDir) -> PathBuf {
    let external = auth_dir.path().parent().unwrap().join("external");
    std::fs::create_dir_all(&external).unwrap();
    let secret = external.join("secret.json");
    std::fs::write(&secret, r#"{"secret":true}"#).unwrap();
    secret
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// The names of the files in the auth directory, sorted.
fn listing(auth_dir: &AuthDir) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(auth_dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Fails without the privilege to make symlinks (or Developer Mode).
#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

// TestUploadAuthFile_PreservesPriorityAttributes
#[tokio::test]
async fn upload_preserves_priority_attributes() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let content = r#"{"type":"codex","email":"midai0530@gmail.com","priority":98}"#;

    upload(&api, &[("codex-midai0530@gmail.com-plus.json", content)])
        .await
        .assert(StatusCode::OK, OK);

    let auth = api
        .manager
        .get("codex-midai0530@gmail.com-plus.json")
        .expect("the uploaded credential");
    assert_eq!(auth.attribute("priority"), Some("98"));
    assert_eq!(auth.metadata["priority"], json!(98));
}

// TestUploadAuthFile_InvokesPostAuthPersistHook
#[tokio::test]
async fn upload_sends_the_file_to_the_service() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let content = r#"{"type":"codex","email":"user@example.com","access_token":"token-123"}"#;

    upload(&api, &[("codex-user@example.com.json", content)])
        .await
        .assert(StatusCode::OK, OK);

    let calls = api.sync.calls();
    let [SyncCall::FileWritten(file)] = calls.as_slice() else {
        panic!("expected one written file, got {calls:?}");
    };
    assert_eq!(
        file.path,
        auth_dir.path().join("codex-user@example.com.json")
    );
    assert_eq!(&*file.data, content.as_bytes());
    assert!(api.manager.get("codex-user@example.com.json").is_some());
}

// TestUploadAuthFile_BatchMultipart
#[tokio::test]
async fn upload_batch_multipart() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let files = [
        (
            "alpha.json",
            r#"{"type":"codex","email":"alpha@example.com"}"#,
        ),
        (
            "beta.json",
            r#"{"type":"claude","email":"beta@example.com"}"#,
        ),
    ];

    upload(&api, &files).await.assert(
        StatusCode::OK,
        r#"{"files":["alpha.json","beta.json"],"status":"ok","uploaded":2}"#,
    );

    for (name, content) in files {
        assert_eq!(read(&auth_dir.path().join(name)), content);
    }
    assert_eq!(api.manager.list().len(), files.len());
}

// TestUploadAuthFile_BatchMultipart_InvalidJSONDoesNotOverwriteExistingFile
#[tokio::test]
async fn upload_batch_multipart_invalid_json_does_not_overwrite_existing_file() {
    let auth_dir = AuthDir::new();
    let existing = r#"{"type":"codex","email":"alpha@example.com"}"#;
    auth_dir.write("alpha.json", existing);
    let api = Api::over(&auth_dir);
    let beta = r#"{"type":"claude","email":"beta@example.com"}"#;

    let answer = upload(
        &api,
        &[("alpha.json", r#"{"type":"codex""#), ("beta.json", beta)],
    )
    .await;

    let body = answer.expect(StatusCode::MULTI_STATUS);
    assert_eq!(body["status"], "partial");
    assert_eq!(body["uploaded"], 1);
    assert_eq!(body["files"], json!(["beta.json"]));
    assert_eq!(body["failed"][0]["name"], "alpha.json");
    let error = body["failed"][0]["error"].as_str().unwrap();
    assert!(error.starts_with("invalid auth file: "), "{error}");
    assert_eq!(read(&auth_dir.path().join("alpha.json")), existing);
    assert_eq!(read(&auth_dir.path().join("beta.json")), beta);
}

// TestDeleteAuthFile_BatchQuery
#[tokio::test]
async fn delete_batch_query() {
    let auth_dir = AuthDir::new();
    for name in ["alpha.json", "beta.json"] {
        auth_dir.write(name, r#"{"type":"codex"}"#);
    }
    let api = Api::over(&auth_dir);

    delete(&api, "?name=alpha.json&name=beta.json", "")
        .await
        .assert(
            StatusCode::OK,
            r#"{"deleted":2,"files":["alpha.json","beta.json"],"status":"ok"}"#,
        );

    assert!(listing(&auth_dir).is_empty());
}

// TestDeleteAuthFile_UsesAuthPathFromManager, changed: a credential whose
// file is outside the auth directory isn't deleted.
#[tokio::test]
async fn delete_refuses_a_credential_file_outside_the_auth_directory() {
    let auth_dir = AuthDir::new();
    let file_name = "codex-user@example.com-plus.json";
    let shadow = auth_dir.write(
        file_name,
        r#"{"type":"codex","email":"shadow@example.com"}"#,
    );
    let external = auth_dir.path().parent().unwrap().join("external");
    std::fs::create_dir(&external).unwrap();
    let real_path = external.join(file_name);
    std::fs::write(&real_path, r#"{"type":"codex","email":"real@example.com"}"#).unwrap();
    let api = Api::over(&auth_dir);
    let mut record = auth(
        &format!("legacy/{file_name}"),
        &[("path", real_path.to_str().unwrap())],
    );
    record.file_name = file_name.into();
    api.manager.register_unsaved(record).unwrap();

    delete_name(&api, file_name).await.assert(
        StatusCode::CONFLICT,
        r#"{"error":"auth file is outside the auth directory"}"#,
    );

    assert!(real_path.exists());
    assert!(shadow.exists());
    assert_eq!(api.files("").await.len(), 1);
    assert!(api.sync.calls().is_empty());
}

// Not upstream's: whether a credential's file is in the auth directory is
// for the file system to say, so a directory whose name Go's case folding
// equates with the auth directory's (a Kelvin sign for `K`) isn't it, and
// the delete is refused, changing nothing.
#[tokio::test]
async fn delete_tells_apart_directories_case_folding_equates() {
    let auth_dir = AuthDir::new();
    let root = auth_dir.path().parent().unwrap().to_path_buf();
    let kelvin = char::from_u32(0x212A).unwrap().to_string();
    let inside = root.join("K");
    let outside = root.join(&kelvin);
    std::fs::create_dir(&inside).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let file_name = "same.json";
    let inside_file = inside.join(file_name);
    let outside_file = outside.join(file_name);
    std::fs::write(&inside_file, CODEX).unwrap();
    std::fs::write(&outside_file, OTHER).unwrap();
    auth_dir.store.set_base_dir(&inside);
    let api = Api::over(&auth_dir);
    let mut record = auth(
        &format!("legacy/{file_name}"),
        &[("path", outside_file.to_str().unwrap())],
    );
    record.file_name = file_name.into();
    api.manager.register_unsaved(record).unwrap();

    delete_name(&api, file_name).await.assert(
        StatusCode::CONFLICT,
        r#"{"error":"auth file is outside the auth directory"}"#,
    );

    assert_eq!(read(&inside_file), CODEX);
    assert_eq!(read(&outside_file), OTHER);
    assert!(api.sync.calls().is_empty());
}

// Not upstream's: a credential whose file is in the auth directory under
// another spelling of its path has that file deleted, and the service is
// told of the path the credential names; one whose directory is gone is
// refused.
#[tokio::test]
async fn delete_resolves_the_credential_file_directory() {
    let auth_dir = AuthDir::new();
    let path = auth_dir.write("spelled.json", CODEX);
    let dir_name = auth_dir.path().file_name().unwrap().to_owned();
    let spelled = auth_dir
        .path()
        .join("..")
        .join(dir_name)
        .join("spelled.json");
    let gone = auth_dir.path().join("gone").join("gone.json");
    let api = Api::over(&auth_dir);
    for (id, file) in [("spelled", &spelled), ("gone", &gone)] {
        api.manager
            .register_unsaved(auth(id, &[("path", file.to_str().unwrap())]))
            .unwrap();
    }

    delete_name(&api, "gone").await.assert(
        StatusCode::CONFLICT,
        r#"{"error":"auth file is outside the auth directory"}"#,
    );
    delete_name(&api, "spelled")
        .await
        .assert(StatusCode::OK, OK);
    assert!(!path.exists());
    let calls = api.sync.calls();
    assert!(
        matches!(calls.as_slice(), [SyncCall::FileRemoved(removed)] if *removed == spelled),
        "{calls:?}"
    );
}

// TestDeleteAuthFile_FallbackToAuthDirPath
#[tokio::test]
async fn delete_falls_back_to_the_auth_dir_path() {
    let auth_dir = AuthDir::new();
    let path = auth_dir.write("fallback-user.json", r#"{"type":"codex"}"#);
    let api = Api::over(&auth_dir);

    delete_name(&api, "fallback-user.json")
        .await
        .assert(StatusCode::OK, OK);

    assert!(!path.exists());
    let calls = api.sync.calls();
    assert!(
        matches!(calls.as_slice(), [SyncCall::FileRemoved(removed)] if *removed == path),
        "{calls:?}"
    );
}

// TestDeleteAuthFile_RemovesRuntimeAuth
#[tokio::test]
async fn delete_removes_runtime_auth() {
    let auth_dir = AuthDir::new();
    let file_name = "runtime-remove-user.json";
    let record = file_auth(
        &auth_dir.path(),
        "runtime-remove-auth",
        file_name,
        r#"{"type":"codex","email":"runtime@example.com"}"#,
    );
    let api = Api::over(&auth_dir);
    api.manager.register_unsaved(record).unwrap();

    delete_name(&api, file_name)
        .await
        .assert(StatusCode::OK, OK);

    assert!(api.manager.get("runtime-remove-auth").is_none());
    assert!(!auth_dir.path().join(file_name).exists());
}

// TestDownloadAuthFile_ReturnsFile
#[tokio::test]
async fn download_returns_file() {
    let auth_dir = AuthDir::new();
    let expected = r#"{"type":"codex"}"#;
    auth_dir.write("download-user.json", expected);
    let api = Api::over(&auth_dir);

    let answer = download(&api, "download-user.json").await;

    answer.assert(StatusCode::OK, expected);
    assert_eq!(answer.header("content-type"), Some("application/json"));
    assert_eq!(
        answer.header("content-disposition"),
        Some(r#"attachment; filename="download-user.json""#)
    );
}

// TestDownloadAuthFile_RejectsPathSeparators
#[tokio::test]
async fn download_rejects_path_separators() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);

    for name in [
        "../external/secret.json",
        r"..\\external\\secret.json",
        "nested/secret.json",
        r"nested\\secret.json",
    ] {
        let answer = download(&api, name).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{name}: {answer:?}");
    }
}

// TestDownloadAuthFile_PreventsWindowsSlashTraversal
#[cfg(windows)]
#[tokio::test]
async fn download_prevents_windows_slash_traversal() {
    let auth_dir = AuthDir::new();
    external_secret(&auth_dir);
    let api = Api::over(&auth_dir);

    let answer = download(&api, "../external/secret.json").await;

    answer.assert(StatusCode::BAD_REQUEST, INVALID_NAME);
}

// Not upstream's: which names are safe, on every system.
#[test]
fn unsafe_names() {
    for name in [
        "",
        " ",
        "a/b.json",
        r"a\b.json",
        "C:x.json",
        r"C:\x.json",
        r"\\server\share\x.json",
        "/etc/x.json",
        "a.json:stream",
        "CON",
        "con.json",
        "NUL.json",
        "nul .json",
        "Aux.tar.json",
        "COM1.json",
        "lpt9.json",
        "COM0.json",
        "LPT\u{b9}.json",
        "com\u{b3}",
        "CONIN$.json",
        "conout$",
        "x.json.",
        "x.json ",
        ".",
        "..",
        "a<b.json",
        "a>b.json",
        "a\"b.json",
        "a|b.json",
        "a?b.json",
        "a*b.json",
        "a\u{1}.json",
        "a\u{1f}.json",
    ] {
        assert!(is_unsafe_name(name), "{name:?} should be unsafe");
    }
    for name in [
        "codex-user@example.com.json",
        "COM10.json",
        "LPT.json",
        "console.json",
        "CONx.json",
        "nul-user.json",
        ".hidden.json",
        " leading.json",
        "a b.json",
        "x.JSON",
        "caf\u{e9}.json",
        "vertex-project-1.json",
    ] {
        assert!(!is_unsafe_name(name), "{name:?} should be safe");
    }
}

/// Names that must never reach a file: paths of every kind, NTFS streams,
/// Windows devices, trailing dots and characters Windows refuses.
const UNSAFE: &[&str] = &[
    "../external/secret.json",
    r"..\external\secret.json",
    "nested/secret.json",
    r"nested\secret.json",
    "C:secret.json",
    r"C:\external\secret.json",
    r"\\server\share\secret.json",
    "//server/share/secret.json",
    "/etc/secret.json",
    "secret.json:stream",
    "secret.json::$DATA",
    "CON.json",
    "nul.json",
    "com1.json",
    "LPT\u{b9}.json",
    "secret.json.",
    "..",
    "a<b.json",
    "a\u{1}.json",
];

// Not upstream's: an unsafe name is refused by every route, and nothing
// outside the auth directory is read, written or removed.
#[tokio::test]
async fn unsafe_names_are_refused() {
    let auth_dir = AuthDir::new();
    let secret = external_secret(&auth_dir);
    let api = Api::over(&auth_dir);
    let absolute = secret.to_str().unwrap().to_owned();

    for name in UNSAFE.iter().copied().chain([absolute.as_str()]) {
        download(&api, name)
            .await
            .assert(StatusCode::BAD_REQUEST, INVALID_NAME);
        upload_raw(&api, name, CODEX)
            .await
            .assert(StatusCode::BAD_REQUEST, INVALID_NAME);
        delete_name(&api, name)
            .await
            .assert(StatusCode::BAD_REQUEST, INVALID_NAME);
    }

    assert_eq!(read(&secret), r#"{"secret":true}"#);
    assert!(listing(&auth_dir).is_empty());
    assert!(api.sync.calls().is_empty());
}

// Not upstream's: an uploaded file is named by what follows the last `/`
// or `\` of its `filename`, and that name must be safe and end in `.json`.
#[tokio::test]
async fn uploaded_file_names_are_checked() {
    let auth_dir = AuthDir::new();
    let secret = external_secret(&auth_dir);
    let api = Api::over(&auth_dir);

    upload(&api, &[("../external/secret.json", CODEX)])
        .await
        .assert(StatusCode::OK, OK);
    assert_eq!(read(&auth_dir.path().join("secret.json")), CODEX);
    assert_eq!(read(&secret), r#"{"secret":true}"#);

    for name in ["C:secret.json", "CON.json", "nul.json", "a<b.json"] {
        upload(&api, &[(name, CODEX)])
            .await
            .assert(StatusCode::BAD_REQUEST, INVALID_NAME);
    }
    for name in ["secret.json:stream", "secret.txt", "secret.json."] {
        upload(&api, &[(name, CODEX)])
            .await
            .assert(StatusCode::BAD_REQUEST, r#"{"error":"file must be .json"}"#);
    }
    assert_eq!(listing(&auth_dir), ["secret.json"]);
}

// Not upstream's: an uploaded file is named as Go names it, from an RFC 2231
// `filename*` (taken over a plain `filename`) and with parameter names in
// any case. A header Go can't parse leaves the part without names, so it
// isn't a file.
#[tokio::test]
async fn uploaded_file_names_are_read_as_go_reads_them() {
    for (disposition, saved) in [
        (
            "form-data; name=\"file\"; filename*=UTF-8''extended.json",
            "extended.json",
        ),
        (
            "form-data; name=\"file\"; filename=\"plain.json\"; filename*=UTF-8''extended.json",
            "extended.json",
        ),
        (
            "form-data; Name=\"file\"; Filename=\"upper.json\"",
            "upper.json",
        ),
    ] {
        let auth_dir = AuthDir::new();
        let api = Api::over(&auth_dir);
        let request = Multipart::new()
            .part(disposition, Some("application/json"), CODEX.as_bytes())
            .request(Method::POST, "/v0/management/auth-files");

        api.send(request).await.assert(StatusCode::OK, OK);
        assert_eq!(listing(&auth_dir), [saved], "{disposition}");
        assert_eq!(read(&auth_dir.path().join(saved)), CODEX);
    }

    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let request = Multipart::new()
        .part(
            "form-data; name=\"file\"; filename=\"a.json\"; filename=\"b.json\"",
            Some("application/json"),
            CODEX.as_bytes(),
        )
        .request(Method::POST, "/v0/management/auth-files");
    api.send(request)
        .await
        .assert(StatusCode::BAD_REQUEST, r#"{"error":"no files uploaded"}"#);
    assert!(listing(&auth_dir).is_empty());
}

/// Each row is a `Content-Disposition` and the field and file names Go
/// 1.26.4's `multipart.Part` (`FormName` and `FileName`) gave for it,
/// recorded by a probe on Windows, whose `filepath.Base` splits at `\` as
/// well as `/`. Bytes other than printable ASCII, and `\`, are written
/// `\xNN`.
#[rustfmt::skip]
const GO_PART_NAMES: &[(&[u8], &[u8], &[u8])] = &[
    (b"form-data; name=\"file\"; filename=\"plain.json\"", b"file", b"plain.json"),
    (b"form-data; name=\"file\"; filename*=UTF-8''extended.json", b"file", b"extended.json"),
    (b"form-data; name=\"file\"; filename=\"plain.json\"; filename*=UTF-8''extended.json", b"file", b"extended.json"),
    (b"form-data; name=\"file\"; filename*=UTF-8''extended.json; filename=\"plain.json\"", b"file", b"extended.json"),
    (b"form-data; Name=\"file\"; Filename=\"upper.json\"", b"file", b"upper.json"),
    (b"Form-Data; NAME=file; FILENAME=upper.json", b"file", b"upper.json"),
    (b"form-data; name=\"file\"; filename*=utf-8'en'%E2%82%AC%20rates.json", b"file", b"\xe2\x82\xac rates.json"),
    (b"form-data; name=\"file\"; filename*=us-ascii''a%41.json", b"file", b"aA.json"),
    (b"form-data; name=\"file\"; filename*=Utf-8''mixed.json", b"file", b"mixed.json"),
    (b"form-data; name=\"file\"; filename*=ISO-8859-1''latin.json", b"file", b""),
    (b"form-data; name=\"file\"; filename=\"plain.json\"; filename*=ISO-8859-1''latin.json", b"file", b"plain.json"),
    (b"form-data; name=\"file\"; filename*=''nocharset.json", b"file", b""),
    (b"form-data; name=\"file\"; filename*=UTF-8'onequote.json", b"file", b""),
    (b"form-data; name=\"file\"; filename*=\"UTF-8''quoted.json\"", b"file", b"quoted.json"),
    (b"form-data; name=\"file\"; filename*=UTF-8''bad%zz.json", b"file", b""),
    (b"form-data; name=\"file\"; filename*=UTF-8''short%4", b"file", b""),
    (b"form-data; name=\"file\"; filename=\"plain.json\"; filename*=UTF-8''", b"file", b""),
    (b"form-data; name=\"file\"; filename*0=\"cont\"; filename*1=\"inued.json\"", b"file", b"continued.json"),
    (b"form-data; name=\"file\"; filename*0*=UTF-8''%63ont; filename*1*=%69nued.json", b"file", b"continued.json"),
    (b"form-data; name=\"file\"; filename*0*=UTF-8''a; filename*2=c.json", b"file", b"a"),
    (b"form-data; name=\"file\"; filename*1=b.json", b"file", b""),
    (b"form-data; name=\"file\"; filename*0*=UTF-8''a; filename*1*=%zz.json", b"file", b"a"),
    (b"form-data; name=\"file\"; filename*0*=latin1''a; filename*1=b.json", b"file", b"b.json"),
    (b"form-data; name=\"file\"; filename=\"plain.json\"; filename*0=zero.json", b"file", b"zero.json"),
    (b"form-data; name=\"file\"; filename*=UTF-8''star.json; filename*0=zero.json", b"file", b"star.json"),
    (b"form-data; name=\"file\"; filename*=UTF-8''a.json; filename*=UTF-8''b.json", b"", b""),
    (b"form-data; name=\"file\"; filename=\"a.json\"; filename=\"b.json\"", b"", b""),
    (b"form-data; name=\"file\"; filename=\"a.json\"; filename=\"a.json\"", b"file", b"a.json"),
    (b"form-data; name=\"file\"; filename=\"a.json\"; FILENAME=\"b.json\"", b"", b""),
    (b"form-data; name=\"file\"; name=\"other\"; filename=\"a.json\"", b"", b""),
    (b"form-data; name=\"file\"; filename=\"a.json", b"", b""),
    (b"form-data; name=\"file\"; filename=a b.json", b"", b""),
    (b"form-data; name=\"file\"; filename=a@b.json", b"", b""),
    (b"form-data; name=\"file\"; filename=\"a.json\";", b"file", b"a.json"),
    (b"form-data; name=\"file\"; filename=\"a.json\" ; ", b"file", b"a.json"),
    (b"form-data; name=\"file\"; filename=\"a.json\";;", b"", b""),
    (b"form-data; name=\"file\"; ; filename=\"a.json\"", b"", b""),
    (b"form-data; name=\"file\"; filename=", b"", b""),
    (b"form-data; name=\"file\"; =a.json", b"", b""),
    (b"attachment; name=\"file\"; filename=\"a.json\"", b"", b"a.json"),
    (b"form-data", b"", b""),
    (b"form-data;", b"", b""),
    (b"", b"", b""),
    (b"form data; name=\"file\"", b"", b""),
    (b"form-data/x; name=\"file\"; filename=\"a.json\"", b"", b"a.json"),
    (b"form-data/; name=\"file\"", b"", b""),
    (b" form-data ; name=\"file\"; filename=\"spaced.json\"", b"file", b"spaced.json"),
    (b"form-data; name=\"\"; filename=\"a.json\"", b"", b"a.json"),
    (b"form-data; name=\"file\"; filename=\"\"", b"file", b""),
    (b"form-data; name=\"file\"; filename=\"dir/sub/a.json\"", b"file", b"a.json"),
    (b"form-data; name=\"file\"; filename=\"dir\x5c\x5ca.json\"", b"file", b"a.json"),
    (b"form-data; name=\"file\"; filename=\"dir\x5ca.json\"", b"file", b"a.json"),
    (b"form-data; name=\"file\"; filename=\"dir/\"", b"file", b"dir"),
    (b"form-data; name=\"file\"; filename=\"/\"", b"file", b"\x5c"),
    (b"form-data; name=\"file\"; filename=\"a\x5c\"b.json\"", b"file", b"a\"b.json"),
    (b"form-data; name=\"file\"; filename=\"a\x5c", b"", b""),
    (b"form-data;name=file;filename=nospace.json", b"file", b"nospace.json"),
    (b"form-data;\x09name=\"file\";\x09filename=\"tab.json\"", b"file", b"tab.json"),
    (b"form-data;\xc2\xa0name=\"file\";\xe3\x80\x80filename=\"unicode-space.json\"", b"file", b"unicode-space.json"),
    (b"form-data; name=\"file\"; filename=\"a.json\"; size=12", b"file", b"a.json"),
    (b"form-data; name*=UTF-8''file; filename=\"star-name.json\"", b"file", b"star-name.json"),
    (b"form-data; name*=UTF-8''%C3%A9; filename=\"e.json\"", b"\xc3\xa9", b"e.json"),
    (b"form-data; name=\"file\"; file*name=x; filename=\"cut.json\"", b"file", b"cut.json"),
    (b"form-data; name=\"file\"; filename*=UTF-8''%FF.json", b"file", b"\xff.json"),
    (b"form-data; name=\"caf\xc3\xa9\"; filename=\"caf\xc3\xa9.json\"", b"caf\xc3\xa9", b"caf\xc3\xa9.json"),
    (b"FORM-DATA; NAME=\"file\"; FILENAME*=UTF-8''shout.json", b"file", b"shout.json"),
    (b"form-data; name=\"file\"; filename*0=a; filename*0=a; filename*1=b.json", b"file", b"ab.json"),
    (b"form-data; name=\"file\"; filename*0=a; filename*0=z; filename*1=b.json", b"", b""),
    (b"form-data; name=\"file\"; filename=x.json; name=\"file\"", b"file", b"x.json"),
    (b"form-data; name=\"file\"; filename=\"..\";", b"file", b".."),
    (b"form-data; name=\"file\"; filename=\".\";", b"file", b"."),
    (b"form-data; name=\"file\"; filename*=\"us-asc\xc4\xb0\xc4\xb0''dotted.json\"", b"file", b"dotted.json"),
    (b"form-data; name=\"file\"; filename*=UTF-8''dir%2Fslash.json", b"file", b"slash.json"),
    (b"form-data; name=\"file\"; filename*=UTF-8''dir%5Cback.json", b"file", b"back.json"),
    (b"form-data; name=\"file\"; filename*=utf-8''b.json; FILENAME*=UTF-8''b.json", b"", b""),
    (b" ; name=\"file\"; filename=\"a.json\"", b"", b""),
    (b"form-data ; name = \"file\" ; filename = \"spaces.json\"", b"file", b"spaces.json"),
    (b"form-data; name=\"file\"; filename=\"tab\x09in.json\"", b"file", b"tab\x09in.json"),
    (b"\xc4\xb0nline; name=\"file\"; filename=\"a.json\"", b"", b"a.json"),
    (b"form-data; name=\"file\"; filename=\"bad\xff.json\"", b"file", b"bad\xff.json"),
    (b"form-data; name=\"f\xc3\"; filename=\"x.json\"", b"f\xc3", b"x.json"),
];

// Not upstream's: a part's names are read from its `Content-Disposition` as
// Go reads them (see `GO_PART_NAMES`): parameter names in any case, RFC 2231
// extended values in UTF-8 or US-ASCII and continuations, and Go's rules for
// duplicates, malformed headers and spaces.
#[test]
fn part_names_are_read_as_go_reads_them() {
    for (header, name, file) in GO_PART_NAMES {
        assert_eq!(
            part_names(header),
            (name.to_vec(), file.to_vec()),
            "{}",
            String::from_utf8_lossy(header)
        );
    }
}

// Not upstream's: on Windows a delete matches a credential's ID and file
// name regardless of case; elsewhere only exactly.
#[tokio::test]
async fn deletes_match_case_as_the_file_system_does() {
    let auth_dir = AuthDir::new();
    let record = file_auth(
        &auth_dir.path(),
        "codex-user.json",
        "codex-user.json",
        CODEX,
    );
    let api = Api::over(&auth_dir);
    api.manager.register_unsaved(record).unwrap();

    let answer = delete_name(&api, "CODEX-User.json").await;

    if cfg!(windows) {
        answer.assert(StatusCode::OK, OK);
        assert!(listing(&auth_dir).is_empty());
        assert!(api.manager.get("codex-user.json").is_none());
    } else {
        answer.assert(StatusCode::NOT_FOUND, r#"{"error":"auth file not found"}"#);
        assert_eq!(listing(&auth_dir), ["codex-user.json"]);
    }
}

// Not upstream's: a download never reads through a symlink, an upload never
// writes through one, and a delete removes the link, not its target.
#[tokio::test]
async fn symlinks_are_never_followed() {
    let auth_dir = AuthDir::new();
    let target = auth_dir.path().parent().unwrap().join("target.json");
    std::fs::write(&target, CODEX).unwrap();
    let link = auth_dir.path().join("link.json");
    if let Err(error) = symlink(&target, &link) {
        eprintln!("skipped: can't make a symlink here: {error}");
        return;
    }
    let api = Api::over(&auth_dir);

    let body = download(&api, "link.json")
        .await
        .expect(StatusCode::INTERNAL_SERVER_ERROR);
    let expected = format!("failed to read file: {} is a symlink", link.display());
    assert_eq!(body, json!({ "error": expected }));

    for answer in [
        upload_raw(&api, "link.json", OTHER).await,
        upload(&api, &[("link.json", OTHER)]).await,
    ] {
        let body = answer.expect(StatusCode::INTERNAL_SERVER_ERROR);
        let error = body["error"].as_str().unwrap();
        assert!(error.starts_with("failed to write file: "), "{error}");
        assert!(error.ends_with("is a symlink"), "{error}");
    }
    assert_eq!(read(&target), CODEX);

    delete_name(&api, "link.json")
        .await
        .assert(StatusCode::OK, OK);
    assert!(listing(&auth_dir).is_empty());
    assert_eq!(read(&target), CODEX);
}

// Not upstream's: as `symlinks_are_never_followed`, with a directory
// junction, which Windows lets anyone make, standing in for the symlink.
#[cfg(windows)]
#[tokio::test]
async fn junctions_are_never_followed() {
    let auth_dir = AuthDir::new();
    let target = auth_dir.path().parent().unwrap().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = auth_dir.path().join("link.json");
    let made = std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(&link)
        .arg(&target)
        .output();
    if !made.as_ref().is_ok_and(|output| output.status.success()) {
        eprintln!("skipped: can't make a junction here: {made:?}");
        return;
    }
    let api = Api::over(&auth_dir);

    let body = download(&api, "link.json")
        .await
        .expect(StatusCode::INTERNAL_SERVER_ERROR);
    let expected = format!("failed to read file: {} is a symlink", link.display());
    assert_eq!(body, json!({ "error": expected }));

    for answer in [
        upload_raw(&api, "link.json", OTHER).await,
        upload(&api, &[("link.json", OTHER)]).await,
    ] {
        let body = answer.expect(StatusCode::INTERNAL_SERVER_ERROR);
        let error = body["error"].as_str().unwrap();
        assert!(error.ends_with("is a symlink"), "{error}");
    }
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    assert!(api.sync.calls().is_empty());
}

// Not upstream's: `?all=true` deletes only the `*.json` files at the top of
// the auth directory, and tells the service of each.
#[tokio::test]
async fn delete_all_removes_only_top_level_json_files() {
    let auth_dir = AuthDir::new();
    let alpha = auth_dir.write("alpha.json", CODEX);
    let beta = auth_dir.write("BETA.JSON", OTHER);
    auth_dir.write("notes.txt", "keep");
    std::fs::create_dir(auth_dir.path().join("nested")).unwrap();
    std::fs::write(auth_dir.path().join("nested").join("gamma.json"), CODEX).unwrap();
    std::fs::create_dir(auth_dir.path().join("dir.json")).unwrap();
    let api = Api::over(&auth_dir);

    for all in ["true", "1", "*"] {
        let answer = delete(&api, &format!("?all={}", encode(all)), "").await;
        let deleted = if all == "true" { 2 } else { 0 };
        answer.assert(
            StatusCode::OK,
            &format!(r#"{{"deleted":{deleted},"status":"ok"}}"#),
        );
    }

    assert_eq!(listing(&auth_dir), ["dir.json", "nested", "notes.txt"]);
    assert!(auth_dir.path().join("nested").join("gamma.json").exists());
    let removed: Vec<PathBuf> = api
        .sync
        .calls()
        .into_iter()
        .map(|call| match call {
            SyncCall::FileRemoved(path) => path,
            other => panic!("unexpected call {other:?}"),
        })
        .collect();
    assert_eq!(removed, [beta, alpha]);
}

// Not upstream's: some files of a batch delete failing answer 207.
#[tokio::test]
async fn partial_delete_answers_207() {
    let auth_dir = AuthDir::new();
    auth_dir.write("alpha.json", CODEX);
    let api = Api::over(&auth_dir);

    delete(&api, "?name=alpha.json&name=missing.json&name=CON.json", "")
        .await
        .assert(
            StatusCode::MULTI_STATUS,
            concat!(
                r#"{"deleted":1,"failed":[{"error":"auth file not found","name":"missing.json"},"#,
                r#"{"error":"invalid name","name":"CON.json"}],"files":["alpha.json"],"#,
                r#""status":"partial"}"#,
            ),
        );
}

// Not upstream's: the files to delete may be listed in the body, as a list
// or an object, when the query names none.
#[tokio::test]
async fn delete_reads_names_from_the_body() {
    let auth_dir = AuthDir::new();
    for name in ["a.json", "b.json", "c.json", "d.json", "e.json"] {
        auth_dir.write(name, CODEX);
    }
    let api = Api::over(&auth_dir);

    delete(&api, "", r#"["a.json", " b.json ", null, "a.json", ""]"#)
        .await
        .assert(
            StatusCode::OK,
            r#"{"deleted":2,"files":["a.json","b.json"],"status":"ok"}"#,
        );
    delete(
        &api,
        "",
        r#" {"name":"c.json","names":["d.json","c.json"]} "#,
    )
    .await
    .assert(
        StatusCode::OK,
        r#"{"deleted":2,"files":["c.json","d.json"],"status":"ok"}"#,
    );
    delete(&api, "", r#"{"NAME":"e.json"}"#)
        .await
        .assert(StatusCode::OK, OK);
    assert!(listing(&auth_dir).is_empty());

    for body in [
        r#"{"name":1}"#,
        r#"{"names":"a.json"}"#,
        r#"{"name":"a.json"} {}"#,
        r#"["a.json""#,
        r#"[1]"#,
        r#""a.json""#,
    ] {
        delete(&api, "", body).await.assert(
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid request body"}"#,
        );
    }
    for body in ["", " ", "{}", "null", "[]", r#"{"name":" "}"#] {
        delete(&api, "", body)
            .await
            .assert(StatusCode::BAD_REQUEST, INVALID_NAME);
    }
}

// Not upstream's: an upload sent as the body, named by `?name=`, is checked
// as a form's file is, and one that holds no credential is never written.
#[tokio::test]
async fn raw_uploads() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);

    upload_raw(&api, " raw.json ", CODEX)
        .await
        .assert(StatusCode::OK, OK);
    assert_eq!(read(&auth_dir.path().join("raw.json")), CODEX);
    assert!(api.manager.get("raw.json").is_some());

    api.post("/v0/management/auth-files", CODEX)
        .await
        .assert(StatusCode::BAD_REQUEST, INVALID_NAME);
    upload_raw(&api, "raw.txt", CODEX).await.assert(
        StatusCode::BAD_REQUEST,
        r#"{"error":"name must end with .json"}"#,
    );

    for (body, reason) in [
        ("null", "not a JSON object"),
        (
            "[1]",
            "json: cannot unmarshal array into Go value of type map[string]interface {}",
        ),
        (
            r#""codex""#,
            "json: cannot unmarshal string into Go value of type map[string]interface {}",
        ),
        (r#"{"email":"x@example.com"}"#, "missing type"),
        (r#"{"type":" "}"#, "missing type"),
        (r#"{"type":"gemini"}"#, "type gemini isn't served"),
    ] {
        upload_raw(&api, "raw.json", body).await.assert(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({ "error": format!("invalid auth file: {reason}") }).to_string(),
        );
    }
    for body in ["", r#"{"type":"codex""#, "{} x"] {
        let answer = upload_raw(&api, "raw.json", body).await;
        let body = answer.expect(StatusCode::INTERNAL_SERVER_ERROR);
        let error = body["error"].as_str().unwrap();
        assert!(error.starts_with("invalid auth file: "), "{error}");
    }
    assert_eq!(read(&auth_dir.path().join("raw.json")), CODEX);
    assert_eq!(api.sync.calls().len(), 1);
}

// Not upstream's: a form with no file, or that isn't a form.
#[tokio::test]
async fn forms_must_hold_files() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);

    let request = Multipart::new()
        .text("file", CODEX)
        .request(Method::POST, "/v0/management/auth-files");
    api.send(request)
        .await
        .assert(StatusCode::BAD_REQUEST, r#"{"error":"no files uploaded"}"#);

    let mut request = keyed(Method::POST, "/v0/management/auth-files", "x");
    request.headers_mut().insert(
        http::header::CONTENT_TYPE,
        "multipart/form-data".parse().unwrap(),
    );
    api.send(request).await.assert(
        StatusCode::BAD_REQUEST,
        r#"{"error":"invalid multipart form: no multipart boundary param in Content-Type"}"#,
    );
    assert!(listing(&auth_dir).is_empty());
}

// Not upstream's: forms and bodies over 32 MiB, files over 8 MiB and forms
// of over 1000 parts are refused, and nothing is written.
#[tokio::test]
async fn uploads_are_bounded() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let too_large = r#"{"error":"request body too large"}"#;
    let huge = vec![b' '; (32 << 20) + 1];
    let large = vec![b' '; (8 << 20) + 1];

    let request =
        form(&[("huge.json", huge.as_slice())]).request(Method::POST, "/v0/management/auth-files");
    api.send(request)
        .await
        .assert(StatusCode::PAYLOAD_TOO_LARGE, too_large);
    let body = String::from_utf8(huge).unwrap();
    upload_raw(&api, "huge.json", &body)
        .await
        .assert(StatusCode::PAYLOAD_TOO_LARGE, too_large);

    let large = String::from_utf8(large).unwrap();
    let file_too_large = r#"{"error":"auth file too large"}"#;
    upload(&api, &[("large.json", &large)])
        .await
        .assert(StatusCode::PAYLOAD_TOO_LARGE, file_too_large);
    upload_raw(&api, "large.json", &large)
        .await
        .assert(StatusCode::PAYLOAD_TOO_LARGE, file_too_large);

    let parts = (0..1001).fold(Multipart::new(), |form, _| form.text("field", "x"));
    api.send(parts.request(Method::POST, "/v0/management/auth-files"))
        .await
        .assert(
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid multipart form: multipart: message too large"}"#,
        );

    assert!(listing(&auth_dir).is_empty());
    assert!(api.sync.calls().is_empty());
}

// Not upstream's: a download of a file that is missing, or over 8 MiB.
#[tokio::test]
async fn downloads_of_missing_and_large_files() {
    let auth_dir = AuthDir::new();
    auth_dir.write("large.json", &" ".repeat((8 << 20) + 1));
    let api = Api::over(&auth_dir);

    download(&api, "missing.json")
        .await
        .assert(StatusCode::NOT_FOUND, r#"{"error":"file not found"}"#);
    download(&api, "notes.txt").await.assert(
        StatusCode::BAD_REQUEST,
        r#"{"error":"name must end with .json"}"#,
    );
    let body = download(&api, "large.json")
        .await
        .expect(StatusCode::INTERNAL_SERVER_ERROR);
    let error = body["error"].as_str().unwrap();
    assert!(error.starts_with("failed to read file: "), "{error}");
}

// Not upstream's: the v8 paths serve the same, and every route needs the
// management key.
#[tokio::test]
async fn v8_paths_and_access() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);

    let request =
        form(&[("v8.json", CODEX.as_bytes())]).request(Method::POST, "/v8/management/credentials");
    api.send(request).await.assert(StatusCode::OK, OK);
    api.get("/v8/management/credentials/download?name=v8.json")
        .await
        .assert(StatusCode::OK, CODEX);
    let listed = api
        .get("/v8/management/credentials")
        .await
        .expect(StatusCode::OK);
    assert_eq!(listed["files"].as_array().map(Vec::len), Some(1));
    api.send(keyed(
        Method::DELETE,
        "/v8/management/credentials?name=v8.json",
        "",
    ))
    .await
    .assert(StatusCode::OK, OK);
    assert!(listing(&auth_dir).is_empty());

    auth_dir.write("kept.json", CODEX);
    // Fewer than the failures that get a client banned.
    for (method, path) in [
        (
            Method::GET,
            "/v0/management/auth-files/download?name=kept.json",
        ),
        (Method::POST, "/v0/management/auth-files?name=kept.json"),
        (Method::DELETE, "/v0/management/auth-files?name=kept.json"),
        (Method::DELETE, "/v8/management/credentials?name=kept.json"),
    ] {
        let answer = api.send(request_from(LOCAL, method, path, OTHER)).await;
        assert_eq!(
            answer.status,
            StatusCode::UNAUTHORIZED,
            "{path}: {answer:?}"
        );
    }
    assert_eq!(read(&auth_dir.path().join("kept.json")), CODEX);
}

// Not upstream's: without a credential store every route answers 503; when
// the service has stopped, the file is written or removed and the route
// answers 503.
#[tokio::test]
async fn store_and_service_unavailable() {
    let api = Api::new();
    let unavailable = r#"{"error":"credential store unavailable"}"#;
    download(&api, "a.json")
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, unavailable);
    upload_raw(&api, "a.json", CODEX)
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, unavailable);
    upload(&api, &[("a.json", CODEX)])
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, unavailable);
    delete_name(&api, "a.json")
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, unavailable);

    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    api.sync.stop();
    let stopped = r#"{"error":"credential sync unavailable: the service has stopped"}"#;
    upload_raw(&api, "a.json", CODEX)
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, stopped);
    assert_eq!(read(&auth_dir.path().join("a.json")), CODEX);
    delete_name(&api, "a.json")
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, stopped);
    assert!(listing(&auth_dir).is_empty());
    auth_dir.write("b.json", CODEX);
    delete(&api, "?all=true", "")
        .await
        .assert(StatusCode::SERVICE_UNAVAILABLE, stopped);
    assert!(listing(&auth_dir).is_empty());
}

// Not upstream's: a batch upload reports the base name of each file that
// failed, the uploaded files in the order of their fields.
#[tokio::test]
async fn batch_upload_order_and_failures() {
    let auth_dir = AuthDir::new();
    let api = Api::over(&auth_dir);
    let request = Multipart::new()
        .file("z", "zeta.json", CODEX.as_bytes())
        .file("a", "dir/alpha.txt", CODEX.as_bytes())
        .file("m", "mu.json", OTHER.as_bytes())
        .text("note", "ignored")
        .request(Method::POST, "/v0/management/auth-files");

    let body = api.send(request).await.expect(StatusCode::MULTI_STATUS);

    assert_eq!(
        body,
        json!({
            "failed": [{"error": "file must be .json", "name": "alpha.txt"}],
            "files": ["mu.json", "zeta.json"],
            "status": "partial",
            "uploaded": 2,
        })
    );
    assert_eq!(listing(&auth_dir), ["mu.json", "zeta.json"]);
}

// Not upstream's: a form's `Debug` shows its names and sizes, never what a
// file or value holds.
#[tokio::test]
async fn form_debug_leaves_out_contents() {
    const MARKER: &str = "SYNTHETIC-PRIVATE-KEY-MARKER";
    let contents = format!(r#"{{"private_key":"{MARKER}"}}"#);
    let request = Multipart::new()
        .text("location", MARKER)
        .file("file", "account.json", contents.as_bytes())
        .request(Method::POST, "/v0/management/vertex/import");
    let form = read_form(request).await.unwrap();

    let file = form.file("file").unwrap();
    for shown in [
        format!("{form:?}"),
        format!("{form:#?}"),
        format!("{file:?}"),
    ] {
        assert!(!shown.contains(MARKER), "{shown}");
        assert!(shown.contains("account.json"), "{shown}");
        assert!(
            shown.contains(&format!("{} bytes", contents.len())),
            "{shown}"
        );
    }
    let shown = format!("{form:?}");
    assert!(shown.contains("location"), "{shown}");
    assert!(
        shown.contains(&format!("{} bytes", MARKER.len())),
        "{shown}"
    );
}
