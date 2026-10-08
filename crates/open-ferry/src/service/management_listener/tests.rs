//! Not upstream's: open-ferry's `management.separate-address`, served on
//! 127.0.0.1 ephemeral ports.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use open_ferry_core::config::{Config, ManagementAddress};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use super::{Separate, bind_separate, changed, serve_both, spawn};
use crate::logging::LogLevel;
use crate::service::{Options, Server, Service, run, serve};

const KEY: (&str, &str) = ("Authorization", "Bearer test-secret");
const CLIENT: (&str, &str) = ("Authorization", "Bearer client-key");
const GIN_404: &str = "404 page not found";

/// A service over `dir` with a client key, a management key and `extra`
/// config.
fn service(dir: &Path, extra: &str) -> Service {
    let text = format!(
        "auth-dir: '{}'\napi-keys: ['client-key']\n{extra}",
        dir.display()
    );
    let config = Config::parse(text).unwrap();
    let mut service = Service::new(
        Arc::new(config),
        dir.join("config.yaml"),
        dir.to_owned(),
        LogLevel::detached(),
    );
    service.register_executors();
    service
}

const KEYED: &str = "remote-management:\n  secret-key: test-secret\n";

/// The two listeners, served.
struct Served {
    main: SocketAddr,
    management: SocketAddr,
    stop: watch::Sender<bool>,
    server: Server,
}

/// Serves `service` as `run` does with a management address, on two
/// 127.0.0.1 ephemeral ports.
async fn start(service: &Service, tls: Option<Arc<rustls::ServerConfig>>) -> Served {
    let main = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (main_addr, management) = (main.local_addr().unwrap(), listener.local_addr().unwrap());
    let separate = Separate {
        address: ManagementAddress {
            host: "127.0.0.1".into(),
            port: management.port(),
        },
        listener,
    };
    let (stop, stopped) = watch::channel(false);
    let server = spawn(service, main, separate, tls, Router::new(), stopped);
    Served {
        main: main_addr,
        management,
        stop,
        server,
    }
}

/// A response, read.
#[derive(Debug, PartialEq)]
struct Answer {
    status: u16,
    body: String,
}

/// Sends `method path` with `headers` over `stream`, and reads the answer.
async fn exchange(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Answer {
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
    for (name, value) in headers {
        let _ = write!(request, "{name}: {value}\r\n");
    }
    request.push_str("Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    Answer {
        status: head.split(' ').nth(1).unwrap().parse().unwrap(),
        body: body.to_owned(),
    }
}

async fn fetch(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)]) -> Answer {
    exchange(
        TcpStream::connect(addr).await.unwrap(),
        method,
        path,
        headers,
    )
    .await
}

/// What a listener answers a request.
#[derive(Clone, Copy, Debug)]
enum Want {
    /// This status.
    Is(u16),
    /// Any status but 404: the route is there.
    Served,
    /// The empty 404 of the management paths, the dashboard app and
    /// `/management.html` while management is off.
    Empty404,
    /// The dashboard API's 404 for a path that isn't one of its routes.
    NotFound,
    /// Gin's 404: no route.
    Gin404,
}

fn check(want: Want, answer: &Answer, what: &str) {
    match want {
        Want::Is(status) => assert_eq!(answer.status, status, "{what}: {}", answer.body),
        Want::Served => assert_ne!(answer.status, 404, "{what}: {}", answer.body),
        Want::Empty404 => assert_eq!((answer.status, answer.body.as_str()), (404, ""), "{what}"),
        Want::NotFound => assert_eq!(
            (answer.status, answer.body.as_str()),
            (404, r#"{"error":"not_found","message":"no such route"}"#),
            "{what}"
        ),
        Want::Gin404 => assert_eq!(
            (answer.status, answer.body.as_str()),
            (404, GIN_404),
            "{what}"
        ),
    }
}

/// A request, with what the proxy's listener and the management address's
/// answer it.
type Route<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)], Want, Want);

#[tokio::test]
async fn each_listener_serves_exactly_its_routes() {
    use Want::{Empty404, Gin404, Is, NotFound, Served};
    let dir = tempfile::tempdir().unwrap();
    let service = service(dir.path(), KEYED);
    let served = start(&service, None).await;

    let routes: &[Route] = &[
        // The proxy's routes, and the server's own.
        ("GET", "/v1/models", &[CLIENT], Is(200), Gin404),
        ("GET", "/v1/models", &[], Is(401), Gin404),
        ("POST", "/v1/chat/completions", &[CLIENT], Served, Gin404),
        ("POST", "/v1/messages", &[CLIENT], Served, Gin404),
        ("POST", "/v1/responses", &[CLIENT], Served, Gin404),
        ("GET", "/v1beta/models", &[CLIENT], Is(200), Gin404),
        ("GET", "/", &[], Is(200), Gin404),
        ("GET", "/healthz", &[], Is(200), Gin404),
        // The main server's OAuth callback pages.
        ("GET", "/anthropic/callback", &[], Served, Gin404),
        ("GET", "/codex/callback", &[], Served, Gin404),
        // The management API.
        ("GET", "/v0/management/config", &[KEY], Empty404, Is(200)),
        ("GET", "/v0/management/config", &[], Empty404, Is(401)),
        (
            "GET",
            "/v0/management/auth-files",
            &[KEY],
            Empty404,
            Is(200),
        ),
        (
            "GET",
            "/v8/management/credentials",
            &[KEY],
            Empty404,
            Is(200),
        ),
        (
            "GET",
            "/v0/management/oauth-callback",
            &[],
            Empty404,
            Served,
        ),
        ("GET", "/v0/management/nothing", &[KEY], Empty404, Empty404),
        ("GET", "/v8/management", &[KEY], Empty404, Empty404),
        // The dashboard and its API.
        ("GET", "/dashboard/", &[], Empty404, Is(200)),
        ("GET", "/dashboard/usage", &[], Empty404, Is(200)),
        ("GET", "/management.html", &[], Empty404, Is(302)),
        (
            "GET",
            "/open-ferry/api/v1/client-setup",
            &[KEY],
            NotFound,
            Is(200),
        ),
        (
            "GET",
            "/open-ferry/api/v1/usage/summary",
            &[KEY],
            NotFound,
            Served,
        ),
        (
            "GET",
            "/open-ferry/api/v1/usage/summary",
            &[],
            NotFound,
            Is(401),
        ),
        (
            "POST",
            "/open-ferry/api/v1/client-setup",
            &[KEY],
            NotFound,
            Is(405),
        ),
        (
            "GET",
            "/open-ferry/api/v1/nothing",
            &[KEY],
            NotFound,
            NotFound,
        ),
        // Neither.
        ("GET", "/nothing", &[], Gin404, Gin404),
        ("GET", "/keep-alive", &[], Gin404, Gin404),
    ];
    for &(method, path, headers, main, management) in routes {
        let what = format!("{method} {path} {headers:?}");
        let answer = fetch(served.main, method, path, headers).await;
        check(main, &answer, &format!("proxy's listener: {what}"));
        let answer = fetch(served.management, method, path, headers).await;
        check(management, &answer, &format!("management listener: {what}"));
    }

    // Both answer CORS preflights, as every path does.
    for addr in [served.main, served.management] {
        let answer = fetch(addr, "OPTIONS", "/v0/management/config", &[]).await;
        assert_eq!(answer.status, 204, "{addr}");
    }
    let _ = served.stop.send(true);
}

// The proxy's listener answers every management and dashboard path as a
// server with no management key and the control panel turned off does, but
// the dashboard API's, which it answers as that server answers a path that
// isn't one of the API's routes.
#[tokio::test]
async fn the_proxys_listener_answers_as_with_management_off() {
    let dir = tempfile::tempdir().unwrap();
    let separate = service(
        dir.path(),
        "remote-management:\n  secret-key: test-secret\n  allow-remote: true\n",
    );
    let served = start(&separate, None).await;
    let off_dir = tempfile::tempdir().unwrap();
    let off = service(
        off_dir.path(),
        "remote-management:\n  disable-control-panel: true\n",
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let off_addr = listener.local_addr().unwrap();
    let (stop_off, stopped) = watch::channel(false);
    tokio::spawn(serve(listener, None, off.app(), stopped));

    for path in [
        "/v0/management",
        "/v0/management/",
        "/v0/management/config",
        "/v0/management/config.yaml",
        "/v0/management/auth-files",
        "/v0/management/api-keys",
        "/v0/management/oauth-callback",
        "/v0/management/nothing",
        "/v8/management/config",
        "/v8/management/credentials",
        "/management.html",
        "/dashboard",
        "/dashboard/",
        "/dashboard/index.html",
        "/dashboard/settings",
        "/open-ferry/api/v1/client-setup",
        "/open-ferry/api/v1/usage/summary",
        "/open-ferry/api/v1/usage/ledger",
        "/open-ferry/api/v1/request-logs",
        "/open-ferry/api/v1/nothing",
        "/open-ferry/nothing",
    ] {
        for method in ["GET", "POST", "PUT", "DELETE"] {
            for headers in [&[][..], &[KEY][..]] {
                let off_path = if path.starts_with("/open-ferry/") {
                    "/open-ferry/api/v1/nothing"
                } else {
                    path
                };
                let want = fetch(off_addr, method, off_path, headers).await;
                let got = fetch(served.main, method, path, headers).await;
                assert_eq!(got, want, "{method} {path} {headers:?}");
            }
        }
    }
    let _ = served.stop.send(true);
    let _ = stop_off.send(true);
}

#[tokio::test]
async fn both_listeners_stop_together() {
    let dir = tempfile::tempdir().unwrap();
    let service = service(dir.path(), KEYED);
    let served = start(&service, None).await;
    assert_eq!(fetch(served.main, "GET", "/healthz", &[]).await.status, 200);
    let answer = fetch(served.management, "GET", "/v0/management/config", &[KEY]).await;
    assert_eq!(answer.status, 200);

    served.stop.send(true).unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(10), served.server).await;
    assert!(
        matches!(stopped, Ok(Ok(Ok(())))),
        "the servers didn't stop: {stopped:?}"
    );
    for addr in [served.main, served.management] {
        let connect = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await;
        assert!(!matches!(connect, Ok(Ok(_))), "{addr} still accepts");
    }
}

#[tokio::test]
async fn a_failing_server_stops_both() {
    let pending = || std::future::pending::<std::io::Result<()>>();
    let failed = |message: &'static str| async move {
        Err::<(), _>(std::io::Error::new(std::io::ErrorKind::AddrInUse, message))
    };

    serve_both(async { Ok(()) }, async { Ok(()) })
        .await
        .unwrap();
    let error = serve_both(pending(), failed("boom")).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    assert_eq!(error.to_string(), "management server: boom");
    let error = serve_both(failed("bad"), pending()).await.unwrap_err();
    assert_eq!(error.to_string(), "bad");
    // One that stopped waits for the other.
    let error = serve_both(async { Ok(()) }, failed("later"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "management server: later");
}

#[tokio::test]
async fn binding_fails_with_a_clear_message() {
    let busy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = busy.local_addr().unwrap().port();
    let mut config = Config::default();
    assert!(bind_separate(&config).unwrap().is_none());

    config.remote_management.separate_address = format!("127.0.0.1:{port}");
    let error = bind_separate(&config).err().expect("the port is taken");
    assert!(
        error.starts_with(&format!(
            "failed to start the management server on 127.0.0.1:{port}: "
        )),
        "{error}"
    );

    // One the loader would refuse, as one made in code may be.
    config.remote_management.separate_address = "127.0.0.1".into();
    assert_eq!(
        bind_separate(&config).err().as_deref(),
        Some(
            "management.separate-address: \"127.0.0.1\" has no port; \
             write it as host:port, such as 127.0.0.1:8318"
        )
    );
    drop(busy);
}

// A start whose management address is taken fails, after binding the
// proxy's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_fails_when_the_management_address_is_taken() {
    let dir = tempfile::tempdir().unwrap();
    let auth_dir = dir.path().join("auth");
    let busy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = busy.local_addr().unwrap().port();
    // Port 0: the proxy's is the system's pick.
    let text = format!(
        "host: 127.0.0.1\nauth-dir: '{}'\n{KEYED}  separate-address: '127.0.0.1:{port}'\n",
        auth_dir.display()
    );
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, &text).unwrap();
    let config = Config::load(&path).unwrap();
    let code = tokio::time::timeout(
        Duration::from_secs(30),
        run(
            config,
            path,
            auth_dir,
            LogLevel::detached(),
            Options::default(),
            std::future::pending(),
        ),
    )
    .await
    .expect("the start failed at once");
    assert_eq!(code, ExitCode::FAILURE);
    drop(busy);
}

// Both listeners use server.tls.
#[tokio::test]
async fn both_listeners_use_tls() {
    let dir = tempfile::tempdir().unwrap();
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert, generated.cert.pem()).unwrap();
    std::fs::write(&key, generated.signing_key.serialize_pem()).unwrap();
    let tls = crate::tls::load(cert.to_str().unwrap(), key.to_str().unwrap()).unwrap();
    let service = service(dir.path(), KEYED);
    let served = start(&service, Some(tls)).await;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(generated.cert.der().clone()).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    for (addr, path, headers) in [
        (served.main, "/healthz", &[][..]),
        (served.management, "/v0/management/config", &[KEY][..]),
    ] {
        let stream = TcpStream::connect(addr).await.unwrap();
        let stream = connector.connect(name.clone(), stream).await.unwrap();
        let answer = exchange(stream, "GET", path, headers).await;
        assert_eq!(answer.status, 200, "{path}: {}", answer.body);
        // Plain HTTP isn't served.
        let mut plain = TcpStream::connect(addr).await.unwrap();
        plain
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        let _ =
            tokio::time::timeout(Duration::from_secs(5), plain.read_to_end(&mut response)).await;
        assert!(!response.starts_with(b"HTTP/1.1 200"), "{path}");
    }
    let _ = served.stop.send(true);
}

#[test]
fn a_change_takes_a_restart() {
    let before = Config::parse("port: 8317\n").unwrap();
    let after =
        Config::parse("port: 8317\nmanagement: {separate-address: '127.0.0.1:8318'}\n").unwrap();
    assert!(changed(&before, &after));
    assert!(changed(&after, &before));
    assert!(!changed(&after, &after.clone()));
    let mut other = before.clone();
    other.remote_management.allow_remote = true;
    assert!(!changed(&before, &other));
}
