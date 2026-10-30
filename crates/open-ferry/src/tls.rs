// Ported from the TLS half of Server.Start in CLIProxyAPI
// internal/api/server.go (v8.0.42, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Serving over TLS when the config's `tls.enable` is on.
//!
//! The certificate chain and key are PEM files, as Go's
//! `tls.LoadX509KeyPair` reads them, and ALPN offers HTTP/2 and HTTP/1.1.
//! They are read once, when the server starts, as upstream reads them.
//! Handlers see a client's address as `ConnectInfo<SocketAddr>`, as over
//! plain HTTP.
//!
//! The handshake is run by the serve loop (`service::serve_loop`), which
//! serves plain and TLS connections alike. `TlsListener` and `TlsPeer`,
//! which fed finished handshakes to `axum::serve` before that loop, are
//! kept, in test builds only, for the test of that path.
//!
//! Deviations from upstream:
//! - Handshakes run on their own tasks with the configured ingress timeout
//!   (ten seconds by default); upstream's server has no handshake timeout.

#[cfg(test)]
use std::io;
#[cfg(test)]
use std::net::SocketAddr;
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

#[cfg(test)]
use axum::Router;
#[cfg(test)]
use axum::extract::Request;
#[cfg(test)]
use axum::extract::connect_info::{ConnectInfo, Connected};
#[cfg(test)]
use axum::middleware::{self, Next};
#[cfg(test)]
use axum::response::Response;
#[cfg(test)]
use axum::serve::IncomingStream;
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
#[cfg(test)]
use tokio::net::TcpListener;
#[cfg(test)]
use tokio::net::TcpStream;
#[cfg(test)]
use tokio::sync::mpsc;
#[cfg(test)]
use tokio_rustls::TlsAcceptor;
#[cfg(test)]
use tokio_rustls::server::TlsStream;

/// How long a client gets to finish its handshake.
#[cfg(test)]
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Reads the certificate chain and key for `tls.cert` and `tls.key`.
pub fn load(cert: &str, key: &str) -> Result<Arc<ServerConfig>, String> {
    let (cert, key) = (cert.trim(), key.trim());
    if cert.is_empty() || key.is_empty() {
        return Err("tls.cert or tls.key is empty".into());
    }
    let chain = CertificateDer::pem_file_iter(cert)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|error| format!("open {cert}: {error}"))?;
    if chain.is_empty() {
        return Err(format!(
            "tls: failed to find any PEM data in certificate input {cert}"
        ));
    }
    let key = PrivateKeyDer::from_pem_file(key).map_err(|error| format!("open {key}: {error}"))?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|error| format!("tls: {error}"))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Runs the TLS handshake with the client at `addr` on `stream`, giving it
/// [`HANDSHAKE_TIMEOUT`]. A handshake that fails or times out is logged
/// and gives `None`.
#[cfg(test)]
pub(crate) async fn handshake(
    acceptor: &TlsAcceptor,
    stream: TcpStream,
    addr: SocketAddr,
) -> Option<TlsStream<TcpStream>> {
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
        Ok(Ok(stream)) => Some(stream),
        Ok(Err(error)) => {
            tracing::debug!("TLS handshake from {addr} failed: {error}");
            None
        }
        Err(_) => {
            tracing::debug!("TLS handshake from {addr} timed out");
            None
        }
    }
}

/// A listener that hands the server connections whose TLS handshake has
/// finished.
#[cfg(test)]
pub struct TlsListener {
    connections: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local_addr: SocketAddr,
}

#[cfg(test)]
impl TlsListener {
    /// Accepts on `listener` with `config`.
    pub fn new(listener: TcpListener, config: Arc<ServerConfig>) -> io::Result<Self> {
        let local_addr = listener.local_addr()?;
        let (sender, connections) = mpsc::channel(64);
        tokio::spawn(accept_loop(listener, TlsAcceptor::from(config), sender));
        Ok(Self {
            connections,
            local_addr,
        })
    }
}

#[cfg(test)]
async fn accept_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    sender: mpsc::Sender<(TlsStream<TcpStream>, SocketAddr)>,
) {
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = sender.closed() => return,
        };
        let (stream, addr) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                // As axum's own listener does, wait out errors such as
                // running out of file descriptors.
                tracing::debug!("accept failed: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        crate::service::send_at_once(&stream);
        let (acceptor, sender) = (acceptor.clone(), sender.clone());
        tokio::spawn(async move {
            if let Some(stream) = handshake(&acceptor, stream, addr).await {
                let _ = sender.send((stream, addr)).await;
            }
        });
    }
}

#[cfg(test)]
impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.connections.recv().await {
            Some(connection) => connection,
            // The accept loop only stops once this listener is gone.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local_addr)
    }
}

/// A TLS client's address, which axum hands to handlers as
/// `ConnectInfo<TlsPeer>`.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub struct TlsPeer(SocketAddr);

#[cfg(test)]
impl Connected<IncomingStream<'_, TlsListener>> for TlsPeer {
    fn connect_info(stream: IncomingStream<'_, TlsListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

/// `app`, with a TLS client's address also given to handlers as
/// `ConnectInfo<SocketAddr>`. Serve it with
/// `into_make_service_with_connect_info::<TlsPeer>()`.
#[cfg(test)]
pub fn with_peer_addr(app: Router) -> Router {
    app.layer(middleware::from_fn(copy_peer_addr))
}

#[cfg(test)]
async fn copy_peer_addr(mut request: Request, next: Next) -> Response {
    if let Some(&ConnectInfo(TlsPeer(addr))) = request.extensions().get::<ConnectInfo<TlsPeer>>() {
        request.extensions_mut().insert(ConnectInfo(addr));
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_files_are_reported() {
        assert_eq!(
            load(" ", "key.pem").unwrap_err(),
            "tls.cert or tls.key is empty"
        );
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("cert.pem");
        let error = load(cert.to_str().unwrap(), "key.pem").unwrap_err();
        assert!(error.starts_with("open "), "{error}");
        std::fs::write(&cert, "not pem").unwrap();
        let error = load(cert.to_str().unwrap(), "key.pem").unwrap_err();
        assert!(error.contains("failed to find any PEM data"), "{error}");
    }

    #[tokio::test]
    async fn serves_over_tls() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
        std::fs::write(&cert, generated.cert.pem()).unwrap();
        std::fs::write(&key, generated.signing_key.serialize_pem()).unwrap();
        let config = load(cert.to_str().unwrap(), key.to_str().unwrap()).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listener = TlsListener::new(listener, config).unwrap();
        let addr = axum::serve::Listener::local_addr(&listener).unwrap();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move {
                format!("hello {}", peer.ip())
            }),
        );
        let app = with_peer_addr(app).into_make_service_with_connect_info::<TlsPeer>();
        tokio::spawn(async move { axum::serve(listener, app).await });

        // A client that stalls its handshake holds up no one.
        let _stalled = TcpStream::connect(addr).await.unwrap();

        let mut roots = rustls::RootCertStore::empty();
        roots.add(generated.cert.der().clone()).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
        let stream = TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut stream = connector.connect(name, stream).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("hello 127.0.0.1"), "{response}");
    }

    // Not upstream's: the same exchange through the production serve loop,
    // which serves TLS itself rather than through `TlsListener`.
    #[tokio::test]
    async fn serves_over_tls_through_the_serve_loop() {
        use crate::service::serve_loop::{ServeControl, run};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
        std::fs::write(&cert, generated.cert.pem()).unwrap();
        std::fs::write(&key, generated.signing_key.serialize_pem()).unwrap();
        let config = load(cert.to_str().unwrap(), key.to_str().unwrap()).unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move {
                format!("hello {}", peer.ip())
            }),
        );
        let control = ServeControl::new();
        let serving = control.clone();
        let server = tokio::spawn(async move { run(listener, Some(config), app, &serving).await });

        // A client that stalls its handshake holds up no one.
        let _stalled = TcpStream::connect(addr).await.unwrap();

        let mut roots = rustls::RootCertStore::empty();
        roots.add(generated.cert.der().clone()).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
        let stream = TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut stream = connector.connect(name, stream).await.unwrap();
        stream
            .write_all(
                b"GET / HTTP/1.1
Host: localhost
Connection: close

",
            )
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("hello 127.0.0.1"), "{response}");

        control.stop();
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("the loop didn't stop")
            .unwrap()
            .unwrap();
    }
}
