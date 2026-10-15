// Ported from CLIProxyAPI internal/cmd/siwc_bind_test.go (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const OUTER: &str = r#"{"type":"codex","access_token":"outer-test","refresh_token":"outer-refresh-test","disabled":true,"priority":7,"unknown":{"keep":1}}"#;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("codex.json");
    std::fs::write(&path, OUTER).unwrap();
    (dir, path)
}

#[test]
fn resolves_bare_and_absolute_paths_and_rejects_invalid_targets() {
    let (dir, path) = fixture();
    assert_eq!(
        resolve(dir.path(), "codex.json").unwrap().path,
        path.clone()
    );
    assert_eq!(
        resolve(dir.path(), path.to_str().unwrap()).unwrap().path,
        path.clone()
    );
    let (outside, outside_path) = fixture();
    assert!(resolve(dir.path(), outside_path.to_str().unwrap()).is_err());
    assert!(resolve(dir.path(), "../codex.json").is_err());
    assert!(resolve(dir.path(), "missing.json").is_err());
    assert!(resolve(Path::new(""), "codex.json").is_err());
    assert!(resolve(dir.path(), "").is_err());
    std::fs::create_dir(dir.path().join("directory.json")).unwrap();
    assert!(resolve(dir.path(), "directory.json").is_err());
    for payload in [
        "null",
        "[]",
        "{}",
        r#"{"type":"claude","access_token":"a","refresh_token":"r"}"#,
        r#"{"type":"codex","access_token":123,"refresh_token":"r"}"#,
        r#"{"type":"codex","access_token":"a","refresh_token":" "}"#,
    ] {
        std::fs::write(&path, payload).unwrap();
        assert!(resolve(dir.path(), "codex.json").is_err());
    }
    drop(outside);
}

#[cfg(windows)]
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/windows_junction.rs"
));

#[cfg(any(unix, windows))]
#[test]
fn resolves_the_real_auth_directory_and_refuses_escaping_links() {
    let (dir, _path) = fixture();
    let (outside, _outside_path) = fixture();
    let alias = dir.path().join("alias");
    #[cfg(windows)]
    junction(dir.path(), &alias);
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
    assert_eq!(
        resolve(&alias, "codex.json").unwrap().path,
        alias.join("codex.json")
    );
    assert_eq!(
        resolve(&alias, alias.join("codex.json").to_str().unwrap())
            .unwrap()
            .path,
        alias.join("codex.json")
    );
    let escape = dir.path().join("escape.json");
    #[cfg(windows)]
    junction(outside.path(), &escape);
    #[cfg(unix)]
    std::os::unix::fs::symlink(_outside_path, &escape).unwrap();
    assert!(resolve(dir.path(), "escape.json").is_err());
    #[cfg(windows)]
    assert!(resolve(dir.path(), escape.join("codex.json").to_str().unwrap()).is_err());
    drop(outside);
}

type Requests = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

async fn tokens_server(fail: bool) -> (String, tokio::task::JoinHandle<()>, Requests) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Requests::default();
    let collected = requests.clone();
    let app = axum::Router::new().route("/api/accounts/oauth/token", axum::routing::post(move |body: String| {
        let collected = collected.clone();
        async move {
        collected.lock().unwrap().push(body);
        if fail {
            (axum::http::StatusCode::BAD_REQUEST, "failed".to_owned())
        } else {
            (axum::http::StatusCode::OK, json!({"access_token":"bound-test","refresh_token":"bound-refresh-test","id_token":"test-id","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}).to_string())
        }
    }}));
    #[allow(clippy::disallowed_methods)]
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), task, requests)
}

fn state(url: &str) -> String {
    url.split('?')
        .nth(1)
        .unwrap()
        .split('&')
        .find_map(|pair| pair.strip_prefix("state="))
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn cli_loopback_bind_preserves_outer_fields_and_reuses_or_replaces_device_ids() {
    let (base, server, requests) = tokens_server(false).await;
    for existing in [
        None,
        Some("invalid-test"),
        Some("123e4567-e89b-42d3-a456-426614174000"),
    ] {
        let (dir, path) = fixture();
        let mut before: Value = serde_json::from_str(OUTER).unwrap();
        if let Some(id) = existing {
            before["siwc"] = json!({"device_id":id,"access_token":"old-test"});
        }
        std::fs::write(&path, before.to_string()).unwrap();
        let target = resolve(dir.path(), "codex.json").unwrap();
        let device = target.device_id.clone();
        assert!(validate_device_id(&device).is_some());
        if existing.is_some_and(|s| validate_device_id(s).is_some()) {
            assert!(existing == Some(device.as_str()));
        }
        let auth = SiwcAuth::with_proxy_url("").with_endpoints(siwc::Endpoints::with_base(&base));
        let result = flow(target, auth, 0, Duration::from_secs(3), |url, port| {
            let callback_state = state(url);
            #[allow(clippy::disallowed_methods)]
            tokio::spawn(async move {
                let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                socket.write_all(format!("GET /auth/callback?code=test-code&state={callback_state}&client_id=issued-test HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                let mut response = Vec::new();
                socket.read_to_end(&mut response).await.unwrap();
                // Showing the callback page avoids its browser grace wait.
                let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                socket.write_all(b"GET /success HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await.unwrap();
                socket.read_to_end(&mut Vec::new()).await.unwrap();
            });
        }, std::future::pending()).await;
        assert!(result.is_ok());
        let mut after: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(after["use_siwc"], true);
        assert!(after["siwc"]["device_id"].as_str() == Some(device.as_str()));
        assert_eq!(after["siwc"]["client_id"], "issued-test");
        after.as_object_mut().unwrap().remove("siwc");
        after.as_object_mut().unwrap().remove("use_siwc");
        before.as_object_mut().unwrap().remove("siwc");
        assert!(after == before, "outer credential fields changed");
    }
    check_requests(&requests, 3);
    server.abort();
}

fn check_requests(requests: &Requests, expected: usize) {
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), expected);
    for body in requests.iter() {
        assert!(body.contains("client_id=issued-test"));
        assert!(!body.contains("device_id") && !body.contains("ext_agent_host_id"));
    }
}

#[tokio::test]
async fn pasted_callbacks_keep_client_id_and_failures_save_nothing() {
    for case in [
        "query",
        "fragment",
        "state",
        "missing-client",
        "exchange",
        "error",
        "timeout",
    ] {
        let (dir, path) = fixture();
        let before = std::fs::read(&path).unwrap();
        let (base, server, requests) = tokens_server(case == "exchange").await;
        let auth = SiwcAuth::with_proxy_url("").with_endpoints(siwc::Endpoints::with_base(&base));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let result = flow(
            resolve(dir.path(), "codex.json").unwrap(),
            auth,
            0,
            Duration::from_millis(80),
            |url, _| {
                if case == "timeout" {
                    return;
                }
                let state = if case == "state" {
                    "wrong-state".into()
                } else {
                    state(url)
                };
                let client = if case == "missing-client" {
                    ""
                } else {
                    "&client_id=issued-test"
                };
                let delimiter = if case == "fragment" { '#' } else { '?' };
                let grant = if case == "error" {
                    "error=denied"
                } else {
                    "code=test-code"
                };
                let pasted = format!("http://127.0.0.1/cb{delimiter}{grant}&state={state}{client}");
                let parsed = parse_oauth_callback(&pasted).unwrap().unwrap();
                let _ = sender.send(parsed);
            },
            async {
                match receiver.await {
                    Ok(callback) => Ok(callback),
                    Err(_) => std::future::pending().await,
                }
            },
        )
        .await;
        if matches!(case, "query" | "fragment") {
            assert!(result.is_ok());
        } else {
            assert!(result.is_err());
            if case == "error" {
                assert!(matches!(
                    result,
                    Err((1, "OAuth callback reported an error"))
                ));
            }
            assert!(std::fs::read(&path).unwrap() == before);
        }
        check_requests(
            &requests,
            usize::from(matches!(case, "query" | "fragment" | "exchange")),
        );
        server.abort();
    }
}

#[tokio::test]
async fn bind_refuses_targets_replaced_or_corrupted_after_selection() {
    for corrupt in [false, true] {
        let (dir, path) = fixture();
        let target = resolve(dir.path(), "codex.json").unwrap();
        if corrupt {
            use std::io::Write as _;
            let mut replacement = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
            replacement.write_all(b"{").unwrap();
            replacement
                .persist(&path)
                .map_err(|error| error.error)
                .unwrap();
        } else {
            std::fs::rename(&path, dir.path().join("old.json")).unwrap();
            std::fs::write(&path, OUTER).unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        assert!(save(target, Map::new()).await.is_err());
        assert!(std::fs::read(&path).unwrap() == before);
    }
}

#[tokio::test]
async fn occupied_and_out_of_range_callback_ports_return_thirteen() {
    let (dir, path) = fixture();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let before = std::fs::read(&path).unwrap();
    let result = flow(
        resolve(dir.path(), "codex.json").unwrap(),
        SiwcAuth::with_proxy_url("")
            .with_endpoints(siwc::Endpoints::with_base("http://127.0.0.1:9")),
        port,
        Duration::from_secs(1),
        |_, _| panic!("occupied port must not present URL"),
        std::future::pending(),
    )
    .await;
    assert!(matches!(
        result,
        Err((13, "the callback port is already in use"))
    ));
    let code = run(
        &Config::default(),
        dir.path(),
        "codex.json",
        Options {
            no_browser: true,
            callback_port: 65536,
        },
    )
    .await;
    assert_eq!(code, ExitCode::from(13));
    assert!(std::fs::read(&path).unwrap() == before);
}

#[tokio::test]
async fn cli_pending_bind_preserves_an_atomic_refresh_of_the_selected_user() {
    let (dir, path) = fixture();
    let mut before: Value = serde_json::from_str(OUTER).unwrap();
    before["account_id"] = json!("workspace-test");
    before["email"] = json!("selected@example.test");
    std::fs::write(&path, before.to_string()).unwrap();
    let target = resolve(dir.path(), "codex.json").unwrap();
    let store = FileStore::new(&target.directory);
    let mut refreshed = target.record.clone();
    refreshed
        .metadata
        .insert("access_token".into(), json!("atomic-refresh-test"));
    let patch = open_ferry_core::auth::codex_refresh::CredentialRefreshPatch::new(
        &target.record.metadata,
        &refreshed.metadata,
        true,
        false,
    );
    let guard = acquire_credential_lock(&target.path).await.unwrap();
    store
        .apply_codex_refresh_locked(&refreshed, &patch, &guard)
        .unwrap();
    drop(guard);
    save(target, Map::new()).await.unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(saved["access_token"] == "atomic-refresh-test");
    assert!(saved["use_siwc"] == true);
}

#[tokio::test]
async fn cli_refuses_leaf_alias_and_real_target_with_alias() {
    let (dir, path) = fixture();
    let target = resolve(dir.path(), "codex.json").unwrap();
    let alias = dir.path().join("alias.json");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    #[cfg(windows)]
    if let Err(error) = std::os::windows::fs::symlink_file(&path, &alias) {
        if error.raw_os_error() == Some(1314) {
            eprintln!("UNVERIFIED: CLI file-symlink fixture unavailable (OS 1314)");
            return;
        }
        panic!("file-symlink fixture failed");
    }
    assert!(resolve(dir.path(), "alias.json").is_err());
    assert!(save(target, Map::new()).await.is_err());
    assert!(std::fs::read(&path).unwrap() == OUTER.as_bytes());
    std::fs::remove_file(&alias).unwrap();
    save(resolve(dir.path(), "codex.json").unwrap(), Map::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn cli_auth_directory_retarget_during_bind_is_refused() {
    let (dir, path) = fixture();
    let (other, other_path) = fixture();
    let parent = tempfile::tempdir().unwrap();
    let alias = parent.path().join("alias");
    #[cfg(windows)]
    junction(dir.path(), &alias);
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
    let target = resolve(&alias, "codex.json").unwrap();
    #[cfg(windows)]
    {
        std::fs::remove_dir(&alias).unwrap();
        junction(other.path(), &alias);
    }
    #[cfg(unix)]
    {
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(other.path(), &alias).unwrap();
    }
    assert!(save(target, Map::new()).await.is_err());
    assert!(std::fs::read(path).unwrap() == OUTER.as_bytes());
    assert!(std::fs::read(other_path).unwrap() == OUTER.as_bytes());
}

#[test]
fn cli_errors_are_fixed_per_cause_without_private_details() {
    for (source, expected) in [
        (
            "SIWC bind target has a linked alias",
            "credential target has a linked alias",
        ),
        (
            "invalid SIWC bind target JSON",
            "credential JSON is invalid",
        ),
        (
            "SIWC bind requires outer tokens",
            "credential is missing outer tokens",
        ),
        (
            "SIWC bind target identity changed",
            "credential identity changed during sign-in",
        ),
        ("private@example.test/path", "credential I/O failed"),
    ] {
        assert_eq!(safe_error(&io::Error::other(source)), expected);
    }
}

#[tokio::test]
async fn cli_preserves_unresolved_paths_for_relative_directories_and_nested_targets() {
    let (dir, path) = fixture();
    let current = std::env::current_dir().unwrap().canonicalize().unwrap();
    let absolute = dir.path().canonicalize().unwrap();
    // A relative path needs a shared ancestor, and CI's Windows scratch
    // directory is on another drive than the checkout.
    if let Some(ancestor) = current
        .ancestors()
        .find(|parent| absolute.starts_with(parent))
    {
        let mut relative = PathBuf::new();
        for _ in ancestor.components().count()..current.components().count() {
            relative.push("..");
        }
        relative.push(absolute.strip_prefix(ancestor).unwrap());
        let target = resolve(&relative, "codex.json").unwrap();
        assert!(target.path == relative.join("codex.json"));
        save(target, Map::new()).await.unwrap();
        assert!(
            serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap()["use_siwc"]
                == true
        );
    } else {
        eprintln!("UNVERIFIED: relative-path bind fixture has no shared ancestor with cwd");
    }
    let nested = dir.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let path = nested.join("codex.json");
    std::fs::write(&path, OUTER).unwrap();
    let target = resolve(dir.path(), path.to_str().unwrap()).unwrap();
    save(target, Map::new()).await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn cli_bind_succeeds_with_directory_junctions_including_a_loop() {
    let (dir, path) = fixture();
    let (outside, outside_path) = fixture();
    junction(outside.path(), &dir.path().join("outside-junction"));
    junction(dir.path(), &dir.path().join("loop-junction"));
    save(resolve(dir.path(), "codex.json").unwrap(), Map::new())
        .await
        .unwrap();
    assert!(
        serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap()["use_siwc"] == true
    );
    assert!(std::fs::read(outside_path).unwrap() == OUTER.as_bytes());
}

#[cfg(windows)]
#[tokio::test]
async fn cli_refuses_alternate_stream_leaf_at_selection_and_save() {
    let (dir, path) = fixture();
    let stream = dir.path().join("codex.json:bind-stream");
    std::fs::write(&stream, OUTER).unwrap();
    let Err(error) = resolve(dir.path(), stream.to_str().unwrap()) else {
        panic!("alternate stream selected");
    };
    assert_eq!(
        safe_error(&error),
        "credential leaf has an alternate spelling"
    );
    let mut target = resolve(dir.path(), "codex.json").unwrap();
    target.path = stream.clone();
    target
        .record
        .attributes
        .insert("path".into(), stream.to_string_lossy().into_owned());
    let Err(error) = save(target, Map::new()).await else {
        panic!("alternate stream saved");
    };
    assert_eq!(
        safe_error(&error),
        "credential leaf has an alternate spelling"
    );
    assert!(std::fs::read(path).unwrap() == OUTER.as_bytes());
    assert!(std::fs::read(stream).unwrap() == OUTER.as_bytes());
}

#[cfg(windows)]
#[tokio::test]
async fn cli_refuses_short_leaf_at_selection_and_save() {
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetShortPathNameW(long: *const u16, short: *mut u16, size: u32) -> u32;
    }
    let (dir, old) = fixture();
    let long = dir.path().join("credential-with-a-long-leaf-name.json");
    std::fs::rename(old, &long).unwrap();
    let wide: Vec<u16> = long.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut output = vec![0u16; 32768];
    // SAFETY: wide is NUL-terminated and output is a writable buffer of size.
    let length =
        unsafe { GetShortPathNameW(wide.as_ptr(), output.as_mut_ptr(), output.len() as u32) };
    assert!(
        length > 0 && (length as usize) < output.len(),
        "short-path fixture query failed"
    );
    let short = PathBuf::from(std::ffi::OsString::from_wide(&output[..length as usize]));
    if short.file_name() == long.file_name() {
        eprintln!("UNVERIFIED: fixture volume does not provide Windows short leaf names");
        return;
    }
    let short = long.parent().unwrap().join(short.file_name().unwrap());
    let Err(error) = resolve(dir.path(), short.to_str().unwrap()) else {
        panic!("short leaf selected");
    };
    assert_eq!(
        safe_error(&error),
        "credential leaf has an alternate spelling"
    );
    let mut target = resolve(dir.path(), long.to_str().unwrap()).unwrap();
    target.path = short.clone();
    target
        .record
        .attributes
        .insert("path".into(), short.to_string_lossy().into_owned());
    let Err(error) = save(target, Map::new()).await else {
        panic!("short leaf saved");
    };
    assert_eq!(
        safe_error(&error),
        "credential leaf has an alternate spelling"
    );
    assert!(std::fs::read(long).unwrap() == OUTER.as_bytes());
}
